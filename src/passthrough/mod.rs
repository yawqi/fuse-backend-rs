// Copyright (C) 2020 Alibaba Cloud. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Fuse passthrough file system, mirroring an existing FS hierarchy.
//!
//! This file system mirrors the existing file system hierarchy of the system, starting at the
//! root file system. This is implemented by just "passing through" all requests to the
//! corresponding underlying file system.
//!
//! The code is derived from the
//! [CrosVM](https://chromium.googlesource.com/chromiumos/platform/crosvm/) project,
//! with heavy modification/enhancements from Alibaba Cloud OS team.

use std::any::Any;
use std::collections::{btree_map, BTreeMap};
use std::ffi::{CStr, CString};
use std::fs::File;
use std::io;
use std::marker::PhantomData;
use std::mem::MaybeUninit;
use std::os::unix::io::{AsRawFd, FromRawFd, RawFd};
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, RwLock, RwLockWriteGuard};
use std::time::Duration;

use vm_memory::ByteValued;

use crate::abi::linux_abi as fuse;
use crate::api::filesystem::Entry;
use crate::api::{BackendFileSystem, VFS_MAX_INO};
use crate::BitmapSlice;

#[cfg(feature = "async-io")]
mod async_io;
mod file_handle;
mod multikey;
mod sync_io;

use file_handle::{FileHandle, MountFds};
use multikey::MultikeyBTreeMap;

use crate::async_util::{AsyncDrive, AsyncDriver};

const CURRENT_DIR_CSTR: &[u8] = b".\0";
const PARENT_DIR_CSTR: &[u8] = b"..\0";
const EMPTY_CSTR: &[u8] = b"\0";
const PROC_CSTR: &[u8] = b"/proc\0";

type Inode = u64;
type Handle = u64;

#[derive(Clone, Copy, PartialOrd, Ord, PartialEq, Eq, Debug)]
enum InodeAltKey {
    Ids {
        ino: libc::ino64_t,
        dev: libc::dev_t,
    },
    Handle(FileHandle),
}

impl InodeAltKey {
    fn ids_from_stat(st: &libc::stat64) -> Self {
        InodeAltKey::Ids {
            ino: st.st_ino,
            dev: st.st_dev,
        }
    }
}

enum FileOrHandle {
    File(File),
    Handle(FileHandle),
}

impl FileOrHandle {
    fn handle(&self) -> Option<&FileHandle> {
        match self {
            FileOrHandle::File(_) => None,
            FileOrHandle::Handle(h) => Some(h),
        }
    }
}

#[derive(Debug)]
enum InodeFile<'a> {
    Owned(File),
    Ref(&'a File),
}

impl AsRawFd for InodeFile<'_> {
    /// Return a file descriptor for this file
    /// Note: This fd is only valid as long as the `InodeFile` exists.
    fn as_raw_fd(&self) -> RawFd {
        match self {
            Self::Owned(file) => file.as_raw_fd(),
            Self::Ref(file_ref) => file_ref.as_raw_fd(),
        }
    }
}

impl InodeFile<'_> {
    fn into_ref(&self) -> &File {
        match self {
            Self::Owned(f) => f,
            Self::Ref(f) => *f,
        }
    }
}

struct InodeData {
    inode: Inode,
    // Most of these aren't actually files but ¯\_(ツ)_/¯.
    file_or_handle: FileOrHandle,
    refcount: AtomicU64,
    altkey: InodeAltKey,
}

impl<'a> InodeData {
    fn new(inode: Inode, f: FileOrHandle, refcount: u64, altkey: InodeAltKey) -> Self {
        InodeData {
            inode,
            file_or_handle: f,
            refcount: AtomicU64::new(refcount),
            altkey: altkey,
        }
    }

    fn get_file(&self, mount_fds: &MountFds) -> io::Result<InodeFile<'_>> {
        match &self.file_or_handle {
            FileOrHandle::File(f) => Ok(InodeFile::Ref(f)),
            FileOrHandle::Handle(h) => {
                let f = h.open_with_mount_fds(mount_fds, libc::O_PATH)?;
                Ok(InodeFile::Owned(f))
            }
        }
    }
}

/// Data structures to manage accessed inodes.
struct InodeMap {
    inodes: RwLock<MultikeyBTreeMap<Inode, InodeAltKey, Arc<InodeData>>>,
}

impl InodeMap {
    fn new() -> Self {
        InodeMap {
            inodes: RwLock::new(MultikeyBTreeMap::new()),
        }
    }

    fn clear(&self) {
        self.inodes.write().unwrap().clear();
    }

    fn get(&self, inode: Inode) -> io::Result<Arc<InodeData>> {
        self.inodes
            .read()
            .unwrap()
            .get(&inode)
            .map(Arc::clone)
            .ok_or_else(ebadf)
    }

    fn get_alt(
        &self,
        ids_altkey: &InodeAltKey,
        handle_altkey: Option<&InodeAltKey>,
    ) -> Option<Arc<InodeData>> {
        let inodes = self.inodes.read().unwrap();

        handle_altkey
            .map(|altkey| inodes.get_alt(altkey))
            .flatten()
            .or_else(|| {
                inodes.get_alt(&ids_altkey).filter(|data| {
                    // When we have to fall back to looking up an inode by its IDs, ensure that
                    // we hit an entry that does not have a file handle.  Entries with file
                    // handles must also have a handle alt key, so if we have not found it by
                    // that handle alt key, we must have found an entry with a mismatching
                    // handle; i.e. an entry for a different file, even though it has the same
                    // inode ID.
                    // (This can happen when we look up a new file that has reused the inode ID
                    // of some previously unlinked inode we still have in `.inodes`.)
                    handle_altkey.is_none() || data.file_or_handle.handle().is_none()
                })
            })
            .map(Arc::clone)
    }

    fn get_map_mut(
        &self,
    ) -> RwLockWriteGuard<MultikeyBTreeMap<Inode, InodeAltKey, Arc<InodeData>>> {
        self.inodes.write().unwrap()
    }

    fn insert(
        &self,
        inode: Inode,
        ids_altkey: InodeAltKey,
        handle_altkey: Option<InodeAltKey>,
        data: InodeData,
    ) {
        let mut inodes = self.get_map_mut();

        inodes.insert(inode, Arc::new(data));
        inodes.insert_alt(ids_altkey, inode);
        if let Some(altkey) = handle_altkey {
            inodes.insert_alt(altkey, inode);
        }
    }
}

struct HandleData {
    inode: Inode,
    file: File,
    lock: Mutex<()>,
}

impl HandleData {
    fn new(inode: Inode, file: File) -> Self {
        HandleData {
            inode,
            file,
            lock: Mutex::new(()),
        }
    }

    fn get_file_mut(&self) -> (MutexGuard<()>, &File) {
        (self.lock.lock().unwrap(), &self.file)
    }

    // When making use of the underlying RawFd, the caller must ensure that the Arc<HandleData>
    // object is within scope. Otherwise it may cause race window to access wrong target fd.
    // By introducing this method, we could explicitly audit all callers making use of the
    // underlying RawFd.
    fn get_handle_raw_fd(&self) -> RawFd {
        self.file.as_raw_fd()
    }
}

struct HandleMap {
    handles: RwLock<BTreeMap<Handle, Arc<HandleData>>>,
}

impl HandleMap {
    fn new() -> Self {
        HandleMap {
            handles: RwLock::new(BTreeMap::new()),
        }
    }

    fn clear(&self) {
        self.handles.write().unwrap().clear();
    }

    fn insert(&self, handle: Handle, data: HandleData) {
        self.handles.write().unwrap().insert(handle, Arc::new(data));
    }

    fn release(&self, handle: Handle, inode: Inode) -> io::Result<()> {
        let mut handles = self.handles.write().unwrap();

        if let btree_map::Entry::Occupied(e) = handles.entry(handle) {
            if e.get().inode == inode {
                // We don't need to close the file here because that will happen automatically when
                // the last `Arc` is dropped.
                e.remove();
                return Ok(());
            }
        }

        Err(ebadf())
    }

    fn get(&self, handle: Handle, inode: Inode) -> io::Result<Arc<HandleData>> {
        self.handles
            .read()
            .unwrap()
            .get(&handle)
            .filter(|hd| hd.inode == inode)
            .map(Arc::clone)
            .ok_or_else(ebadf)
    }
}

#[repr(C, packed)]
#[derive(Clone, Copy, Debug, Default)]
struct LinuxDirent64 {
    d_ino: libc::ino64_t,
    d_off: libc::off64_t,
    d_reclen: libc::c_ushort,
    d_ty: libc::c_uchar,
}
unsafe impl ByteValued for LinuxDirent64 {}

/// The caching policy that the file system should report to the FUSE client. By default the FUSE
/// protocol uses close-to-open consistency. This means that any cached contents of the file are
/// invalidated the next time that file is opened.
#[derive(Debug, Clone, PartialEq)]
pub enum CachePolicy {
    /// The client should never cache file data and all I/O should be directly forwarded to the
    /// server. This policy must be selected when file contents may change without the knowledge of
    /// the FUSE client (i.e., the file system does not have exclusive access to the directory).
    Never,

    /// The client is free to choose when and how to cache file data. This is the default policy and
    /// uses close-to-open consistency as described in the enum documentation.
    Auto,

    /// The client should always cache file data. This means that the FUSE client will not
    /// invalidate any cached data that was returned by the file system the last time the file was
    /// opened. This policy should only be selected when the file system has exclusive access to the
    /// directory.
    Always,
}

impl FromStr for CachePolicy {
    type Err = &'static str;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "never" | "Never" | "NEVER" | "none" | "None" | "NONE" => Ok(CachePolicy::Never),
            "auto" | "Auto" | "AUTO" => Ok(CachePolicy::Auto),
            "always" | "Always" | "ALWAYS" => Ok(CachePolicy::Always),
            _ => Err("invalid cache policy"),
        }
    }
}

impl Default for CachePolicy {
    fn default() -> Self {
        CachePolicy::Auto
    }
}

/// Options that configure the behavior of the passthrough fuse file system.
#[derive(Debug, Clone, PartialEq)]
pub struct Config {
    /// How long the FUSE client should consider directory entries to be valid. If the contents of a
    /// directory can only be modified by the FUSE client (i.e., the file system has exclusive
    /// access), then this should be a large value.
    ///
    /// The default value for this option is 5 seconds.
    pub entry_timeout: Duration,

    /// How long the FUSE client should consider file and directory attributes to be valid. If the
    /// attributes of a file or directory can only be modified by the FUSE client (i.e., the file
    /// system has exclusive access), then this should be set to a large value.
    ///
    /// The default value for this option is 5 seconds.
    pub attr_timeout: Duration,

    /// The caching policy the file system should use. See the documentation of `CachePolicy` for
    /// more details.
    pub cache_policy: CachePolicy,

    /// Whether the file system should enabled writeback caching. This can improve performance as it
    /// allows the FUSE client to cache and coalesce multiple writes before sending them to the file
    /// system. However, enabling this option can increase the risk of data corruption if the file
    /// contents can change without the knowledge of the FUSE client (i.e., the server does **NOT**
    /// have exclusive access). Additionally, the file system should have read access to all files
    /// in the directory it is serving as the FUSE client may send read requests even for files
    /// opened with `O_WRONLY`.
    ///
    /// Therefore callers should only enable this option when they can guarantee that: 1) the file
    /// system has exclusive access to the directory and 2) the file system has read permissions for
    /// all files in that directory.
    ///
    /// The default value for this option is `false`.
    pub writeback: bool,

    /// The path of the root directory.
    ///
    /// The default is `/`.
    pub root_dir: String,

    /// Whether the file system should support Extended Attributes (xattr). Enabling this feature may
    /// have a significant impact on performance, especially on write parallelism. This is the result
    /// of FUSE attempting to remove the special file privileges after each write request.
    ///
    /// The default value for this options is `false`.
    pub xattr: bool,

    /// To be compatible with Vfs and PseudoFs, PassthroughFs needs to prepare
    /// root inode before accepting INIT request.
    ///
    /// The default value for this option is `true`.
    pub do_import: bool,

    /// Control whether no_open is allowed.
    ///
    /// The default value for this option is `false`.
    pub no_open: bool,

    /// Control whether no_opendir is allowed.
    ///
    /// The default value for this option is `false`.
    pub no_opendir: bool,

    /// Control whether kill_priv_v2 is enabled.
    ///
    /// The default value for this option is `false`.
    pub killpriv_v2: bool,

    /// Whether to use file handles to reference inodes.  We need to be able to open file
    /// descriptors for arbitrary inodes, and by default that is done by storing an `O_PATH` FD in
    /// `InodeData`.  Not least because there is a maximum number of FDs a process can have open
    /// users may find it preferable to store a file handle instead, which we can use to open an FD
    /// when necessary.
    /// So this switch allows to choose between the alternatives: When set to `false`, `InodeData`
    /// will store `O_PATH` FDs.  Otherwise, we will attempt to generate and store a file handle
    /// instead.
    ///
    /// The default is `false`.
    pub inode_file_handles: bool,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            entry_timeout: Duration::from_secs(5),
            attr_timeout: Duration::from_secs(5),
            cache_policy: Default::default(),
            writeback: false,
            root_dir: String::from("/"),
            xattr: false,
            do_import: true,
            no_open: false,
            no_opendir: false,
            killpriv_v2: false,
            inode_file_handles: false,
        }
    }
}

/// A file system that simply "passes through" all requests it receives to the underlying file
/// system.
///
/// To keep the implementation simple it servers the contents of its root directory. Users
/// that wish to serve only a specific directory should set up the environment so that that
/// directory ends up as the root of the file system process. One way to accomplish this is via a
/// combination of mount namespaces and the pivot_root system call.
pub struct PassthroughFs<D: AsyncDrive = AsyncDriver, S: BitmapSlice + Send + Sync = ()> {
    // File descriptors for various points in the file system tree. These fds are always opened with
    // the `O_PATH` option so they cannot be used for reading or writing any data. See the
    // documentation of the `O_PATH` flag in `open(2)` for more details on what one can and cannot
    // do with an fd opened with this flag.
    inode_map: InodeMap,
    next_inode: AtomicU64,

    // File descriptors for open files and directories. Unlike the fds in `inodes`, these _can_ be
    // used for reading and writing data.
    handle_map: HandleMap,
    next_handle: AtomicU64,

    // Maps mount IDs to an open FD on the respective ID for the purpose of open_by_handle_at().
    mount_fds: MountFds,

    // File descriptor pointing to the `/proc` directory. This is used to convert an fd from
    // `inodes` into one that can go into `handles`. This is accomplished by reading the
    // `self/fd/{}` symlink. We keep an open fd here in case the file system tree that we are meant
    // to be serving doesn't have access to `/proc`.
    proc: File,

    // Whether writeback caching is enabled for this directory. This will only be true when
    // `cfg.writeback` is true and `init` was called with `FsOptions::WRITEBACK_CACHE`.
    writeback: AtomicBool,

    // Whether no_open is enabled.
    no_open: AtomicBool,

    // Whether no_opendir is enabled.
    no_opendir: AtomicBool,

    // Whether kill_priv_v2 is enabled.
    killpriv_v2: AtomicBool,

    cfg: Config,

    phantom: PhantomData<D>,
    phantom2: PhantomData<S>,
}

impl<D: AsyncDrive, S: BitmapSlice + Send + Sync> PassthroughFs<D, S> {
    /// Create a Passthrough file system instance.
    pub fn new(cfg: Config) -> io::Result<PassthroughFs<D, S>> {
        // Safe because this is a constant value and a valid C string.
        let proc_cstr = unsafe { CStr::from_bytes_with_nul_unchecked(PROC_CSTR) };
        let proc = Self::open_file(
            libc::AT_FDCWD,
            proc_cstr,
            libc::O_PATH | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            0,
        )?;

        Ok(PassthroughFs {
            inode_map: InodeMap::new(),
            next_inode: AtomicU64::new(fuse::ROOT_ID + 1),

            handle_map: HandleMap::new(),
            next_handle: AtomicU64::new(1),
            mount_fds: MountFds::new(),

            proc,

            writeback: AtomicBool::new(false),
            no_open: AtomicBool::new(false),
            no_opendir: AtomicBool::new(false),
            killpriv_v2: AtomicBool::new(false),
            cfg,

            phantom: PhantomData,
            phantom2: PhantomData,
        })
    }

    /// Initialize the Passthrough file system.
    pub fn import(&self) -> io::Result<()> {
        let root = CString::new(self.cfg.root_dir.as_str()).expect("CString::new failed");

        let handle = if self.cfg.inode_file_handles {
            FileHandle::from_name_at_with_mount_fds(
                &libc::AT_FDCWD,
                &root,
                &self.mount_fds,
                |fd, flags| {
                    let pathname = CString::new(format!("self/fd/{}", fd.as_raw_fd()))
                        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
                    Self::open_file(self.proc.as_raw_fd(), &pathname, flags, 0)
                },
            )
        } else {
            Err(io::Error::from_raw_os_error(libc::ENOTSUP))
        };

        // Ignore errors, because having a handle is optional
        let file_or_handle = if let Ok(h) = handle {
            FileOrHandle::Handle(h)
        } else {
            // We use `O_PATH` because we just want this for traversing the directory tree
            // and not for actually reading the contents.
            let f = Self::open_file(
                libc::AT_FDCWD,
                &root,
                libc::O_PATH | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                0,
            )?;
            FileOrHandle::File(f)
        };

        let st = match &file_or_handle {
            FileOrHandle::File(f) => Self::stat(f, None)?,
            FileOrHandle::Handle(_) => Self::stat(&libc::AT_FDCWD, Some(&root))?,
        };

        // Safe because this doesn't modify any memory and there is no need to check the return
        // value because this system call always succeeds. We need to clear the umask here because
        // we want the client to be able to set all the bits in the mode.
        unsafe { libc::umask(0o000) };

        let ids_altkey = InodeAltKey::ids_from_stat(&st);

        // Note that this will always be `None` if `cfg.inode_file_handles` is false, but we only
        // really need this alt key when we do not have an `O_PATH` fd open for every inode.  So if
        // `cfg.inode_file_handles` is false, we do not need this key anyway.
        let handle_altkey = file_or_handle.handle().map(|h| InodeAltKey::Handle(*h));

        // Not sure why the root inode gets a refcount of 2 but that's what libfuse does.
        self.inode_map.insert(
            fuse::ROOT_ID,
            ids_altkey,
            handle_altkey,
            InodeData::new(fuse::ROOT_ID, file_or_handle, 2, ids_altkey),
        );

        Ok(())
    }

    /// Get the list of file descriptors which should be reserved across live upgrade.
    pub fn keep_fds(&self) -> Vec<RawFd> {
        vec![self.proc.as_raw_fd()]
    }

    fn stat(dir: &impl AsRawFd, path: Option<&CStr>) -> io::Result<libc::stat64> {
        // Safe because this is a constant value and a valid C string.
        let pathname =
            path.unwrap_or_else(|| unsafe { CStr::from_bytes_with_nul_unchecked(EMPTY_CSTR) });
        let mut st = MaybeUninit::<libc::stat64>::zeroed();

        // Safe because the kernel will only write data in `st` and we check the return value.
        let res = unsafe {
            libc::fstatat64(
                dir.as_raw_fd(),
                pathname.as_ptr(),
                st.as_mut_ptr(),
                libc::AT_EMPTY_PATH | libc::AT_SYMLINK_NOFOLLOW,
            )
        };
        if res >= 0 {
            // Safe because the kernel guarantees that the struct is now fully initialized.
            Ok(unsafe { st.assume_init() })
        } else {
            Err(io::Error::last_os_error())
        }
    }

    fn create_file_excl(
        dfd: i32,
        pathname: &CStr,
        flags: i32,
        mode: u32,
    ) -> io::Result<Option<File>> {
        let fd = unsafe {
            libc::openat(
                dfd,
                pathname.as_ptr(),
                flags & libc::O_CREAT & libc::O_EXCL,
                mode,
            )
        };
        if fd < 0 {
            let err = io::Error::last_os_error();
            if err.kind() == io::ErrorKind::NotFound {
                return Ok(None);
            }
            return Err(err);
        }
        Ok(Some(unsafe { File::from_raw_fd(fd) }))
    }

    fn open_file(dfd: i32, pathname: &CStr, flags: i32, mode: u32) -> io::Result<File> {
        let fd = if flags & libc::O_CREAT == libc::O_CREAT {
            unsafe { libc::openat(dfd, pathname.as_ptr(), flags, mode) }
        } else {
            unsafe { libc::openat(dfd, pathname.as_ptr(), flags) }
        };

        if fd < 0 {
            return Err(io::Error::last_os_error());
        }

        // Safe because we just opened this fd.
        Ok(unsafe { File::from_raw_fd(fd) })
    }

    fn open_proc_file(proc: &File, fd: RawFd, flags: i32) -> io::Result<File> {
        let pathname = CString::new(format!("self/fd/{}", fd))
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;

        // We don't really check `flags` because if the kernel can't handle poorly specified flags
        // then we have much bigger problems. Also, clear the `O_NOFOLLOW` flag if it is set since
        // we need to follow the `/proc/self/fd` symlink to get the file.
        Self::open_file(
            proc.as_raw_fd(),
            &pathname,
            (flags | libc::O_CLOEXEC) & (!libc::O_NOFOLLOW),
            0,
        )
    }

    fn do_lookup(&self, parent: Inode, name: &CStr) -> io::Result<Entry> {
        let p = self.inode_map.get(parent)?;
        let p_file = p.get_file(&self.mount_fds)?;

        let handle = if self.cfg.inode_file_handles {
            FileHandle::from_name_at_with_mount_fds(&p_file, name, &self.mount_fds, |fd, flags| {
                Self::open_proc_file(&self.proc, fd.as_raw_fd(), flags)
            })
        } else {
            Err(io::Error::from_raw_os_error(libc::ENOTSUP))
        };

        // Ignore errors, because having a handle is optional
        let file_or_handle = if let Ok(h) = handle {
            FileOrHandle::Handle(h)
        } else {
            let f = Self::open_file(
                p_file.as_raw_fd(),
                name,
                libc::O_PATH | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                0,
            )?;

            FileOrHandle::File(f)
        };

        let st = match &file_or_handle {
            FileOrHandle::File(f) => Self::stat(f, None)?,
            FileOrHandle::Handle(_) => Self::stat(&p_file, Some(name))?,
        };

        let ids_altkey = InodeAltKey::ids_from_stat(&st);

        // Note that this will always be `None` if `cfg.inode_file_handles` is false, but we only
        // really need this alt key when we do not have an `O_PATH` fd open for every inode.  So if
        // `cfg.inode_file_handles` is false, we do not need this key anyway.
        let handle_altkey = file_or_handle.handle().map(|h| InodeAltKey::Handle(*h));

        let mut found = None;
        'search: loop {
            match self.inode_map.get_alt(&ids_altkey, handle_altkey.as_ref()) {
                // No existing entry found
                None => break 'search,
                Some(data) => {
                    let curr = data.refcount.load(Ordering::Acquire);
                    // forgot_one() has just destroyed the entry, retry...
                    if curr == 0 {
                        continue 'search;
                    }

                    // Saturating add to avoid integer overflow, it's not realistic to saturate u64.
                    let new = curr.saturating_add(1);

                    // Synchronizes with the forgot_one()
                    if data
                        .refcount
                        .compare_exchange(curr, new, Ordering::AcqRel, Ordering::Acquire)
                        .is_ok()
                    {
                        found = Some(data.inode);
                        break;
                    }
                }
            }
        }

        let inode = if let Some(v) = found {
            v
        } else {
            // Lookup inode_map again after acquiring the inode_map lock, as there might be another
            // racing thread already added an inode with the same altkey while we're not holding
            // the lock. If so just use the newly added inode, otherwise the inode will be replaced
            // and results in EBADF.
            match self.inode_map.get_alt(&ids_altkey, handle_altkey.as_ref()) {
                Some(data) => {
                    trace!(
                        "fuse: do_lookup sees existing inode {} ids_altkey {:?}",
                        data.inode,
                        ids_altkey
                    );
                    data.refcount.fetch_add(1, Ordering::Relaxed);
                    data.inode
                }
                None => {
                    let inode = self.next_inode.fetch_add(1, Ordering::Relaxed);
                    if inode > VFS_MAX_INO {
                        error!("fuse: max inode number reached: {}", VFS_MAX_INO);
                        return Err(io::Error::new(
                            io::ErrorKind::Other,
                            format!("max inode number reached: {}", VFS_MAX_INO),
                        ));
                    }
                    trace!(
                        "fuse: do_lookup adds new inode {} ids_altkey {:?}",
                        inode,
                        ids_altkey
                    );

                    self.inode_map.insert(
                        inode,
                        ids_altkey,
                        handle_altkey,
                        InodeData::new(inode, file_or_handle, 1, ids_altkey),
                    );
                    inode
                }
            }
        };

        Ok(Entry {
            inode,
            generation: 0,
            attr: st,
            attr_timeout: self.cfg.attr_timeout,
            entry_timeout: self.cfg.entry_timeout,
        })
    }

    fn forget_one(
        inodes: &mut MultikeyBTreeMap<Inode, InodeAltKey, Arc<InodeData>>,
        inode: Inode,
        count: u64,
    ) {
        // ROOT_ID should not be forgotten, or we're not able to access to files any more.
        if inode == fuse::ROOT_ID {
            return;
        }

        if let Some(data) = inodes.get(&inode) {
            // Acquiring the write lock on the inode map prevents new lookups from incrementing the
            // refcount but there is the possibility that a previous lookup already acquired a
            // reference to the inode data and is in the process of updating the refcount so we need
            // to loop here until we can decrement successfully.
            loop {
                let curr = data.refcount.load(Ordering::Acquire);

                // Saturating sub because it doesn't make sense for a refcount to go below zero and
                // we don't want misbehaving clients to cause integer overflow.
                let new = curr.saturating_sub(count);

                trace!(
                    "fuse: forget inode {} refcount {}, count {}, new_count {}",
                    inode,
                    curr,
                    count,
                    new
                );

                // Synchronizes with the acquire load in `do_lookup`.
                if data
                    .refcount
                    .compare_exchange(curr, new, Ordering::AcqRel, Ordering::Acquire)
                    .is_ok()
                {
                    if new == 0 {
                        // We just removed the last refcount for this inode.
                        inodes.remove(&inode);
                    }
                    break;
                }
            }
        }
    }

    fn do_release(&self, inode: Inode, handle: Handle) -> io::Result<()> {
        self.handle_map.release(handle, inode)
    }
}

#[cfg(not(feature = "async-io"))]
impl<D: AsyncDrive, S: 'static + BitmapSlice + Send + Sync> BackendFileSystem<D, S>
    for PassthroughFs<D, S>
{
    fn mount(&self) -> io::Result<(Entry, u64)> {
        let entry = self.do_lookup(fuse::ROOT_ID, &CString::new(".").unwrap())?;
        Ok((entry, VFS_MAX_INO))
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

fn ebadf() -> io::Error {
    io::Error::from_raw_os_error(libc::EBADF)
}
