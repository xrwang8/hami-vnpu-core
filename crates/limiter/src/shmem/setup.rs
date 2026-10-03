use std::ffi::CString;
use std::mem;
use std::ptr;
use std::path::Path;

use libc::{close, fstat, ftruncate, mmap, open, MAP_FAILED, MAP_SHARED, O_CREAT, O_EXCL, O_RDWR, PROT_READ, PROT_WRITE, EEXIST, c_uint, off_t};

use std::sync::atomic::Ordering;

use crate::shmem::{GlobalRegistry, MAX_MANAGERS};

/// Create a file-backed shared memory region.
/// `path` is a full filesystem path; parent directories are created if missing.
pub fn create_shmem<T>(path: &str) -> &'static T {
    // Ensure parent directory exists
    if let Some(parent) = Path::new(path).parent() {
        std::fs::create_dir_all(parent).ok();
    }

    unsafe {
        let c_path = CString::new(path).unwrap();
        let fd = open(c_path.as_ptr(), O_CREAT | O_RDWR, 0o666 as c_uint);
        if fd < 0 { panic!("Manager failed to create shmem file {}: {}", path, std::io::Error::last_os_error()); }

        let size = mem::size_of::<T>();
        if ftruncate(fd, size as off_t) < 0 {
            panic!("Failed to ftruncate {}: {}", path, std::io::Error::last_os_error());
        }

        let ptr = mmap(ptr::null_mut(), size, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
        if ptr == MAP_FAILED { panic!("Failed to mmap {}: {}", path, std::io::Error::last_os_error()); }

        close(fd);
        &*(ptr as *const T)
    }
}

/// Open an existing file-backed shared memory (panics on failure).
pub fn open_shmem<T>(path: &str) -> &'static T {
    try_open_shmem(path).expect("Worker failed to open NPU Manager shmem! Is the Daemon running?")
}

/// Non-panicking version: returns None if the shmem file is not available
/// or not yet fully sized.
pub fn try_open_shmem<T>(path: &str) -> Option<&'static T> {
    unsafe {
        let c_path = CString::new(path).unwrap();
        let fd = open(c_path.as_ptr(), O_RDWR);
        if fd < 0 { return None; }

        // The manager creates the file with open(O_CREAT) then ftruncate()s it; a
        // worker may open it in between. Guard against mapping a too-small file,
        // which would mmap fine but SIGBUS on later access.
        let mut st: libc::stat = mem::zeroed();
        if fstat(fd, &mut st) < 0 || (st.st_size as usize) < mem::size_of::<T>() {
            close(fd);
            return None;
        }

        let ptr = mmap(ptr::null_mut(), mem::size_of::<T>(), PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
        close(fd);
        if ptr == MAP_FAILED { return None; }
        Some(&*(ptr as *const T))
    }
}

pub fn open_global_registry(path: &str) -> &'static GlobalRegistry {
    if let Some(parent) = Path::new(path).parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)
                .unwrap_or_else(|e| panic!("cannot create global registry directory {}: {e}", parent.display()));
        }
    }

    let c_path = CString::new(path).unwrap();
    let size = std::mem::size_of::<GlobalRegistry>();

    // Only the process that atomically creates the file may resize and initialize it.
    // A plain O_CREAT open has a window where another process can map a zero-byte file.
    let (fd, is_creator) = unsafe {
        let creator = open(
            c_path.as_ptr(),
            O_RDWR | O_CREAT | O_EXCL,
            0o666 as c_uint,
        );
        if creator >= 0 {
            (creator, true)
        } else if *libc::__errno_location() != EEXIST {
            panic!("cannot create global registry {}: {}", path, std::io::Error::last_os_error());
        } else {
            let existing = open(c_path.as_ptr(), O_RDWR);
            if existing < 0 {
                panic!("cannot open global registry {}: {}", path, std::io::Error::last_os_error());
            }
            (existing, false)
        }
    };

    if is_creator {
        let result = unsafe { ftruncate(fd, size as off_t) };
        if result < 0 {
            unsafe { close(fd); }
            panic!("failed to size global registry {}: {}", path, std::io::Error::last_os_error());
        }
    } else {
        // The creator publishes the file size only after ftruncate completes. Do not map
        // until that publication is visible, otherwise a later access can SIGBUS.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            let mut stat: libc::stat = unsafe { std::mem::zeroed() };
            let sized = unsafe { fstat(fd, &mut stat) == 0 && stat.st_size as usize >= size };
            if sized {
                break;
            }
            if std::time::Instant::now() >= deadline {
                unsafe { close(fd); }
                panic!("timed out waiting for global registry {} to be sized", path);
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }

    let ptr = unsafe {
        mmap(
            std::ptr::null_mut(),
            size,
            PROT_READ | PROT_WRITE,
            MAP_SHARED,
            fd,
            0,
        )
    };

    // The mapping keeps the file alive; the descriptor is no longer needed.
    unsafe { close(fd); }
    if ptr == MAP_FAILED {
        panic!("mmap failed for global registry {}: {}", path, std::io::Error::last_os_error());
    }

    let reg = unsafe { &*(ptr as *const GlobalRegistry) };
    if is_creator {
        reg.lock_owner.store(MAX_MANAGERS as u32, Ordering::Release);
    }
    reg
}
