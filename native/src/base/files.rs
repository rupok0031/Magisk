use mem::MaybeUninit;
use std::cmp::min;
use std::ffi::CStr;
use std::fs::File;
use std::io::{BufRead, Read, Seek, SeekFrom, Write};
use std::marker::PhantomData;
use std::ops::Deref;
use std::os::fd::{AsFd, BorrowedFd, IntoRawFd};
use std::os::unix::io::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::{io, mem, ptr, slice};

use libc::{c_char, c_uint, dirent, mode_t, EEXIST, ENOENT, O_CLOEXEC, O_PATH, O_RDONLY, O_RDWR};

use crate::{bfmt_cstr, copy_cstr, cstr, errno, error, LibcReturn};

pub fn __open_fd_impl(path: &CStr, flags: i32, mode: mode_t) -> io::Result<OwnedFd> {
    unsafe {
        let fd = libc::open(path.as_ptr(), flags, mode as c_uint).check_os_err()?;
        Ok(OwnedFd::from_raw_fd(fd))
    }
}

#[macro_export]
macro_rules! open_fd {
    ($path:expr, $flags:expr) => {
        $crate::__open_fd_impl($path, $flags, 0)
    };
    ($path:expr, $flags:expr, $mode:expr) => {
        $crate::__open_fd_impl($path, $flags, $mode)
    };
}

pub unsafe fn readlink_unsafe(path: *const c_char, buf: *mut u8, bufsz: usize) -> isize {
    let r = libc::readlink(path, buf.cast(), bufsz - 1);
    if r >= 0 {
        *buf.offset(r) = b'\0';
    }
    r
}

pub fn readlink(path: &CStr, data: &mut [u8]) -> io::Result<usize> {
    let r =
        unsafe { readlink_unsafe(path.as_ptr(), data.as_mut_ptr(), data.len()) }.check_os_err()?;
    Ok(r as usize)
}

pub fn fd_path(fd: RawFd, buf: &mut [u8]) -> io::Result<usize> {
    let mut fd_buf = [0_u8; 40];
    let fd_path = bfmt_cstr!(&mut fd_buf, "/proc/self/fd/{}", fd);
    readlink(fd_path, buf)
}

// Inspired by https://android.googlesource.com/platform/bionic/+/master/libc/bionic/realpath.cpp
pub fn realpath(path: &CStr, buf: &mut [u8]) -> io::Result<usize> {
    let fd = open_fd!(path, O_PATH | O_CLOEXEC)?;
    let mut st1: libc::stat;
    let mut st2: libc::stat;
    let mut skip_check = false;
    unsafe {
        st1 = mem::zeroed();
        if libc::fstat(fd.as_raw_fd(), &mut st1) < 0 {
            // This shall only fail on Linux < 3.6
            skip_check = true;
        }
    }
    let len = fd_path(fd.as_raw_fd(), buf)?;
    unsafe {
        st2 = mem::zeroed();
        if libc::stat(buf.as_ptr().cast(), &mut st2) < 0
            || (!skip_check && (st2.st_dev != st1.st_dev || st2.st_ino != st1.st_ino))
        {
            *errno() = ENOENT;
            return Err(io::Error::last_os_error());
        }
    }
    Ok(len)
}

pub fn mkdirs(path: &CStr, mode: mode_t) -> io::Result<()> {
    let mut buf = [0_u8; 4096];
    let len = copy_cstr(&mut buf, path);
    let buf = &mut buf[..len];
    let mut off = 1;
    unsafe {
        while let Some(p) = buf[off..].iter().position(|c| *c == b'/') {
            buf[off + p] = b'\0';
            if libc::mkdir(buf.as_ptr().cast(), mode) < 0 && *errno() != EEXIST {
                return Err(io::Error::last_os_error());
            }
            buf[off + p] = b'/';
            off += p + 1;
        }
        if libc::mkdir(buf.as_ptr().cast(), mode) < 0 && *errno() != EEXIST {
            return Err(io::Error::last_os_error());
        }
    }
    *errno() = 0;
    Ok(())
}

pub trait ReadExt {
    fn skip(&mut self, len: usize) -> io::Result<()>;
}

impl<T: Read> ReadExt for T {
    fn skip(&mut self, mut len: usize) -> io::Result<()> {
        let mut buf = MaybeUninit::<[u8; 4096]>::uninit();
        let buf = unsafe { buf.assume_init_mut() };
        while len > 0 {
            let l = min(buf.len(), len);
            self.read_exact(&mut buf[..l])?;
            len -= l;
        }
        Ok(())
    }
}

pub trait ReadSeekExt {
    fn skip(&mut self, len: usize) -> io::Result<()>;
}

impl<T: Read + Seek> ReadSeekExt for T {
    fn skip(&mut self, len: usize) -> io::Result<()> {
        if self.seek(SeekFrom::Current(len as i64)).is_err() {
            // If the file is not actually seekable, fallback to read
            ReadExt::skip(self, len)?;
        }
        Ok(())
    }
}

pub trait BufReadExt {
    fn foreach_lines<F: FnMut(&mut String) -> bool>(&mut self, f: F);
    fn foreach_props<F: FnMut(&str, &str) -> bool>(&mut self, f: F);
}

impl<T: BufRead> BufReadExt for T {
    fn foreach_lines<F: FnMut(&mut String) -> bool>(&mut self, mut f: F) {
        let mut buf = String::new();
        loop {
            match self.read_line(&mut buf) {
                Ok(0) => break,
                Ok(_) => {
                    if !f(&mut buf) {
                        break;
                    }
                }
                Err(e) => {
                    error!("{}", e);
                    break;
                }
            };
            buf.clear();
        }
    }

    fn foreach_props<F: FnMut(&str, &str) -> bool>(&mut self, mut f: F) {
        self.foreach_lines(|line| {
            let line = line.trim();
            if line.starts_with('#') {
                return true;
            }
            if let Some((key, value)) = line.split_once('=') {
                return f(key, value);
            }
            true
        });
    }
}

pub trait WriteExt {
    fn write_zeros(&mut self, len: usize) -> io::Result<()>;
}

impl<T: Write> WriteExt for T {
    fn write_zeros(&mut self, mut len: usize) -> io::Result<()> {
        let buf = [0_u8; 4096];
        while len > 0 {
            let l = min(buf.len(), len);
            self.write_all(&buf[..l])?;
            len -= l;
        }
        Ok(())
    }
}

pub struct DirEntry<'a> {
    dir: &'a Directory<'a>,
    entry: &'a dirent,
    d_name_len: usize,
}

impl DirEntry<'_> {
    pub fn d_name(&self) -> &CStr {
        unsafe {
            CStr::from_bytes_with_nul_unchecked(slice::from_raw_parts(
                self.d_name.as_ptr().cast(),
                self.d_name_len,
            ))
        }
    }

    pub fn path(&self, buf: &mut [u8]) -> io::Result<usize> {
        let mut len = self.dir.path(buf)?;
        buf[len] = b'/';
        len += 1;
        len += copy_cstr(&mut buf[len..], self.d_name());
        Ok(len)
    }

    pub fn is_dir(&self) -> bool {
        self.d_type == libc::DT_DIR
    }

    pub fn is_file(&self) -> bool {
        self.d_type == libc::DT_REG
    }

    pub fn is_lnk(&self) -> bool {
        self.d_type == libc::DT_LNK
    }

    pub fn unlink(&self) -> io::Result<()> {
        let flag = if self.is_dir() { libc::AT_REMOVEDIR } else { 0 };
        unsafe {
            libc::unlinkat(self.dir.as_raw_fd(), self.d_name.as_ptr(), flag).check_os_err()?;
        }
        Ok(())
    }

    pub fn open_as_dir(&self) -> io::Result<Directory> {
        if !self.is_dir() {
            return Err(io::Error::from(io::ErrorKind::NotADirectory));
        }
        unsafe {
            let fd = libc::openat(
                self.dir.as_raw_fd(),
                self.d_name.as_ptr(),
                O_RDONLY | O_CLOEXEC,
            )
            .check_os_err()?;
            Directory::try_from(OwnedFd::from_raw_fd(fd))
        }
    }

    pub fn open_as_file(&self, flags: i32) -> io::Result<File> {
        if self.is_dir() {
            return Err(io::Error::from(io::ErrorKind::IsADirectory));
        }
        unsafe {
            let fd = libc::openat(
                self.dir.as_raw_fd(),
                self.d_name.as_ptr(),
                flags | O_CLOEXEC,
            )
            .check_os_err()?;
            Ok(File::from_raw_fd(fd))
        }
    }
}

impl Deref for DirEntry<'_> {
    type Target = dirent;

    fn deref(&self) -> &dirent {
        self.entry
    }
}

pub struct Directory<'a> {
    dirp: *mut libc::DIR,
    _phantom: PhantomData<&'a libc::DIR>,
}

pub enum WalkResult {
    Continue,
    Abort,
    Skip,
}

impl<'a> Directory<'a> {
    pub fn open(path: &CStr) -> io::Result<Directory> {
        let dirp = unsafe { libc::opendir(path.as_ptr()) }.check_os_err()?;
        Ok(Directory {
            dirp,
            _phantom: PhantomData,
        })
    }

    pub fn read(&mut self) -> io::Result<Option<DirEntry<'_>>> {
        *errno() = 0;
        let e = unsafe { libc::readdir(self.dirp) };
        if e.is_null() {
            return if *errno() != 0 {
                Err(io::Error::last_os_error())
            } else {
                Ok(None)
            };
        }
        // Skip both "." and ".."
        unsafe {
            let entry = &*e;
            let d_name = CStr::from_ptr(entry.d_name.as_ptr());
            return if d_name == cstr!(".") || d_name == cstr!("..") {
                self.read()
            } else {
                let e = DirEntry {
                    dir: self,
                    entry,
                    d_name_len: d_name.to_bytes_with_nul().len(),
                };
                Ok(Some(e))
            };
        }
    }

    pub fn rewind(&mut self) {
        unsafe { libc::rewinddir(self.dirp) }
    }

    pub fn path(&self, buf: &mut [u8]) -> io::Result<usize> {
        fd_path(self.as_raw_fd(), buf)
    }

    pub fn post_order_walk<F: FnMut(&DirEntry) -> io::Result<WalkResult>>(
        &mut self,
        mut f: F,
    ) -> io::Result<WalkResult> {
        self.post_order_walk_impl(&mut f)
    }

    pub fn pre_order_walk<F: FnMut(&DirEntry) -> io::Result<WalkResult>>(
        &mut self,
        mut f: F,
    ) -> io::Result<WalkResult> {
        self.pre_order_walk_impl(&mut f)
    }

    pub fn remove_all(&mut self) -> io::Result<()> {
        self.post_order_walk(|e| {
            e.unlink()?;
            Ok(WalkResult::Continue)
        })?;
        Ok(())
    }
}

impl Directory<'_> {
    fn post_order_walk_impl<F: FnMut(&DirEntry) -> io::Result<WalkResult>>(
        &mut self,
        f: &mut F,
    ) -> io::Result<WalkResult> {
        use WalkResult::*;
        loop {
            match self.read()? {
                None => return Ok(Continue),
                Some(ref e) => {
                    if e.is_dir() {
                        let mut dir = e.open_as_dir()?;
                        if let Abort = dir.post_order_walk_impl(f)? {
                            return Ok(Abort);
                        }
                    }
                    match f(e)? {
                        Abort => return Ok(Abort),
                        Skip => return Ok(Continue),
                        Continue => {}
                    }
                }
            }
        }
    }

    fn pre_order_walk_impl<F: FnMut(&DirEntry) -> io::Result<WalkResult>>(
        &mut self,
        f: &mut F,
    ) -> io::Result<WalkResult> {
        use WalkResult::*;
        loop {
            match self.read()? {
                None => return Ok(Continue),
                Some(ref e) => match f(e)? {
                    Abort => return Ok(Abort),
                    Skip => return Ok(Continue),
                    Continue => {
                        if e.is_dir() {
                            let mut dir = e.open_as_dir()?;
                            if let Abort = dir.pre_order_walk_impl(f)? {
                                return Ok(Abort);
                            }
                        }
                    }
                },
            }
        }
    }
}

impl TryFrom<OwnedFd> for Directory<'_> {
    type Error = io::Error;

    fn try_from(fd: OwnedFd) -> io::Result<Self> {
        let dirp = unsafe { libc::fdopendir(fd.into_raw_fd()) }.check_os_err()?;
        Ok(Directory {
            dirp,
            _phantom: PhantomData,
        })
    }
}

impl AsRawFd for Directory<'_> {
    fn as_raw_fd(&self) -> RawFd {
        unsafe { libc::dirfd(self.dirp) }
    }
}

impl<'a> AsFd for Directory<'a> {
    fn as_fd(&self) -> BorrowedFd<'a> {
        unsafe { BorrowedFd::borrow_raw(self.as_raw_fd()) }
    }
}

impl Drop for Directory<'_> {
    fn drop(&mut self) {
        unsafe {
            libc::closedir(self.dirp);
        }
    }
}

pub fn rm_rf(path: &CStr) -> io::Result<()> {
    unsafe {
        let mut stat: libc::stat = mem::zeroed();
        libc::lstat(path.as_ptr(), &mut stat).check_os_err()?;
        if stat.is_dir() {
            let mut dir = Directory::open(path)?;
            dir.remove_all()?;
        }
        libc::remove(path.as_ptr()).check_os_err()?;
    }
    Ok(())
}

pub trait StatExt {
    fn get_mode(&self) -> u32;
    fn get_type(&self) -> u32;

    fn is_dir(&self) -> bool;
    fn is_blk(&self) -> bool;
}

impl StatExt for libc::stat {
    fn get_mode(&self) -> u32 {
        self.st_mode & libc::S_IFMT as u32
    }

    fn get_type(&self) -> u32 {
        self.st_mode & !(libc::S_IFMT as u32)
    }

    fn is_dir(&self) -> bool {
        self.get_type() == libc::S_IFDIR as u32
    }

    fn is_blk(&self) -> bool {
        self.get_type() == libc::S_IFBLK as u32
    }
}

pub trait FdExt {
    fn size(&self) -> io::Result<usize>;
}

const BLKGETSIZE64: u32 = 0x80081272;

impl<T: AsRawFd> FdExt for T {
    fn size(&self) -> io::Result<usize> {
        unsafe fn inner(fd: RawFd) -> io::Result<usize> {
            extern "C" {
                // Don't use the declaration from the libc crate as request should be u32 not i32
                fn ioctl(fd: RawFd, request: u32, ...) -> i32;
            }
            let mut stat: libc::stat = mem::zeroed();
            libc::fstat(fd, &mut stat).check_os_err()?;
            if stat.is_blk() {
                let mut sz = 0_u64;
                ioctl(fd, BLKGETSIZE64, &mut sz).check_os_err()?;
                Ok(sz as usize)
            } else {
                Ok(stat.st_size as usize)
            }
        }

        unsafe { inner(self.as_raw_fd()) }
    }
}

pub struct MappedFile(&'static mut [u8]);

impl MappedFile {
    pub fn open(path: &CStr) -> io::Result<MappedFile> {
        Ok(MappedFile(map_file(path, false)?))
    }

    pub fn open_rw(path: &CStr) -> io::Result<MappedFile> {
        Ok(MappedFile(map_file(path, true)?))
    }

    pub fn create(fd: BorrowedFd, sz: usize, rw: bool) -> io::Result<MappedFile> {
        Ok(MappedFile(map_fd(fd, sz, rw)?))
    }
}

impl AsRef<[u8]> for MappedFile {
    fn as_ref(&self) -> &[u8] {
        self.0
    }
}

impl AsMut<[u8]> for MappedFile {
    fn as_mut(&mut self) -> &mut [u8] {
        self.0
    }
}

impl Drop for MappedFile {
    fn drop(&mut self) {
        unsafe {
            libc::munmap(self.0.as_mut_ptr().cast(), self.0.len());
        }
    }
}

// We mark the returned slice static because it is valid until explicitly unmapped
pub(crate) fn map_file(path: &CStr, rw: bool) -> io::Result<&'static mut [u8]> {
    let flag = if rw { O_RDONLY } else { O_RDWR };
    let fd = open_fd!(path, flag | O_CLOEXEC)?;
    map_fd(fd.as_fd(), fd.size()?, rw)
}

pub(crate) fn map_fd(fd: BorrowedFd, sz: usize, rw: bool) -> io::Result<&'static mut [u8]> {
    let flag = if rw {
        libc::MAP_SHARED
    } else {
        libc::MAP_PRIVATE
    };
    unsafe {
        let ptr = libc::mmap(
            ptr::null_mut(),
            sz,
            libc::PROT_READ | libc::PROT_WRITE,
            flag,
            fd.as_raw_fd(),
            0,
        );
        if ptr == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }
        Ok(slice::from_raw_parts_mut(ptr.cast(), sz))
    }
}
