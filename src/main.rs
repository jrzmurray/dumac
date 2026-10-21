use parking_lot::Mutex;
use rayon::prelude::*;
use rustc_hash::FxHashSet;
use std::cell::RefCell;
use std::env;
use std::ffi::CString;
use std::path::Path;
use std::sync::LazyLock;

// macOS-specific constants not in libc crate
const ATTR_CMN_ERROR: u32 = 0x20000000;
const VNON: u32 = 0;
const VREG: u32 = 1;
const VDIR: u32 = 2;
const VLNK: u32 = 5;

// Max file handles open
const MAX_FILE_HANDLES: usize = 224;

// Sharded inode tracking
const SHARD_COUNT: usize = 128;

// Size of the attribute buffer handed to getattrlistbulk
const ATTR_BUF_SIZE: usize = 128 * 1024;

// Directory contents
#[derive(Debug)]
struct DirInfo {
    // Blocks used by the files in this directory (hardlinks already deduplicated)
    blocks: i64,
    // Full paths of the subdirectories
    subdirs: Vec<String>,
}

thread_local! {
    // Reused per-thread so we don't zero 128 KiB for every directory
    static ATTR_BUF: RefCell<Vec<u8>> = RefCell::new(vec![0u8; ATTR_BUF_SIZE]);
}

// Global sharded inode set for hardlink deduplication
static SEEN_INODES: LazyLock<[Mutex<FxHashSet<u64>>; SHARD_COUNT]> =
    LazyLock::new(|| std::array::from_fn(|_| Mutex::new(FxHashSet::default())));

fn shard_for_inode(inode: u64) -> usize {
    ((inode >> 8) % SHARD_COUNT as u64) as usize
}

// Clear all seen inodes (for testing)
#[cfg(test)]
pub fn clear_seen_inodes() {
    for shard in SEEN_INODES.iter() {
        shard.lock().clear();
    }
}

// Returns the blocks to add (blocks if newly seen, 0 if already seen)
fn check_and_add_inode(inode: u64, blocks: i64) -> i64 {
    let shard_idx = shard_for_inode(inode);
    let mut seen = SEEN_INODES[shard_idx].lock();
    if seen.insert(inode) {
        blocks // Inode was newly added, count the blocks
    } else {
        0 // Inode already seen, don't count
    }
}

// Convert bytes to 512-byte blocks (du default)
fn blocks_from_bytes(bytes: i64) -> i64 {
    (bytes + 511) / 512
}

// Convert blocks to human readable format (du -h style)
fn format_size(blocks: i64) -> String {
    let bytes = blocks * 512;

    if bytes < 1024 {
        format!("{}B", bytes)
    } else if bytes < 1024 * 1024 {
        let kb = bytes as f64 / 1024.0;
        if kb.fract() == 0.0 {
            format!("{}K", kb as i64)
        } else {
            format!("{:.1}K", kb)
        }
    } else if bytes < 1024 * 1024 * 1024 {
        let mb = bytes as f64 / (1024.0 * 1024.0);
        if mb.fract() == 0.0 {
            format!("{}M", mb as i64)
        } else {
            format!("{:.1}M", mb)
        }
    } else if bytes < 1024_i64.pow(4) {
        let gb = bytes as f64 / (1024.0 * 1024.0 * 1024.0);
        if gb.fract() == 0.0 {
            format!("{}G", gb as i64)
        } else {
            format!("{:.1}G", gb)
        }
    } else {
        let tb = bytes as f64 / (1024.0 * 1024.0 * 1024.0 * 1024.0);
        if tb.fract() == 0.0 {
            format!("{}T", tb as i64)
        } else {
            format!("{:.1}T", tb)
        }
    }
}

fn is_dot_or_dotdot(filename: &str) -> bool {
    filename == "." || filename == ".."
}

fn has_glob_chars(arg: &str) -> bool {
    arg.contains(['*', '?', '['])
}

// Expand glob patterns in the arguments. Arguments that exist literally, or that
// contain no glob characters, are passed through untouched. Returns the paths to
// measure and any error messages (patterns with no matches, invalid patterns).
pub fn expand_args(args: &[String]) -> (Vec<String>, Vec<String>) {
    let options = glob::MatchOptions {
        case_sensitive: true,
        require_literal_separator: true,
        require_literal_leading_dot: true,
    };
    let mut paths = Vec::new();
    let mut errors = Vec::new();

    for arg in args {
        if !has_glob_chars(arg) || Path::new(arg).exists() {
            paths.push(arg.clone());
            continue;
        }
        match glob::glob_with(arg, options) {
            Ok(matches) => {
                let before = paths.len();
                paths.extend(
                    matches
                        .filter_map(Result::ok)
                        .map(|p| p.to_string_lossy().into_owned()),
                );
                if paths.len() == before {
                    errors.push(format!("{}: No such file or directory", arg));
                }
            }
            Err(e) => errors.push(format!("{}: invalid pattern: {}", arg, e.msg)),
        }
    }
    (paths, errors)
}

fn main() {
    let args: Vec<String> = env::args().collect();

    if args.len() < 2 {
        eprintln!("usage: {} path [path ...]", args[0]);
        std::process::exit(1);
    }

    let (roots, expand_errors) = expand_args(&args[1..]);
    let mut failed = !expand_errors.is_empty();
    for e in &expand_errors {
        eprintln!("{}: {}", args[0], e);
    }

    let max_threads = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
        // Limit to MAX_FILE_HANDLES to avoid file handle limit errors
        .min(MAX_FILE_HANDLES);
    let pool = rayon::ThreadPoolBuilder::new()
        // Larger than default stack size to handle deep directory trees
        .stack_size(4 * 1024 * 1024)
        .num_threads(max_threads)
        .build()
        .expect("Failed to build thread pool");

    for root in roots {
        match pool.install(|| size_of_path(root.clone())) {
            Ok(total_blocks) => println!("{}\t{}", format_size(total_blocks), root),
            Err(e) => {
                eprintln!("{}: {}", args[0], e);
                failed = true;
            }
        }
    }

    if failed {
        std::process::exit(1);
    }
}

// Size of a command line argument: directories are walked, anything else is
// measured directly. Unreadable paths fall through to calculate_size for its error.
pub fn size_of_path(path: String) -> Result<i64, String> {
    let c_path = CString::new(path.as_str()).map_err(|_| format!("{}: Invalid path", path))?;
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    if unsafe { libc::stat(c_path.as_ptr(), &mut st) } == 0
        && (st.st_mode & libc::S_IFMT) != libc::S_IFDIR
    {
        let blocks = if st.st_nlink > 1 {
            check_and_add_inode(st.st_ino, st.st_blocks)
        } else {
            st.st_blocks
        };
        return Ok(blocks);
    }
    calculate_size(path)
}

// Calculate total size recursively using rayon work stealing
pub fn calculate_size(root_dir: String) -> Result<i64, String> {
    let dir_info = get_dir_info(&root_dir)?;

    // Process subdirectories in parallel
    let subdir_size: i64 = if dir_info.subdirs.is_empty() {
        0
    } else {
        dir_info
            .subdirs
            .into_par_iter()
            .map(|subdir_path| match calculate_size(subdir_path) {
                Ok(size) => size,
                Err(e) => {
                    eprintln!("dumac: {}", e);
                    0
                }
            })
            .sum()
    };

    Ok(dir_info.blocks + subdir_size)
}

fn get_dir_info(path: &str) -> Result<DirInfo, String> {
    // Open directory
    let c_path = CString::new(path).map_err(|_| format!("{}: Invalid path", path))?;
    let dirfd = unsafe { libc::open(c_path.as_ptr(), libc::O_RDONLY) };
    if dirfd == -1 {
        let errno = unsafe { *libc::__error() };
        let error_msg = match errno {
            libc::ENOENT => "No such file or directory",
            libc::EACCES => "Permission denied",
            libc::ENOTDIR => "Not a directory",
            _ => "Cannot access directory",
        };
        return Err(format!("{}: {}", path, error_msg));
    }

    let result = ATTR_BUF.with(|buf| read_dir_entries(path, dirfd, &mut buf.borrow_mut()));

    // Close directory
    unsafe {
        libc::close(dirfd);
    }
    result
}

fn read_dir_entries(path: &str, dirfd: libc::c_int, attrbuf: &mut [u8]) -> Result<DirInfo, String> {
    // Set up attribute list for getattrlistbulk
    let mut attrlist = libc::attrlist {
        bitmapcount: libc::ATTR_BIT_MAP_COUNT as u16,
        reserved: 0,
        commonattr: libc::ATTR_CMN_RETURNED_ATTRS
            | libc::ATTR_CMN_NAME
            | ATTR_CMN_ERROR
            | libc::ATTR_CMN_OBJTYPE
            | libc::ATTR_CMN_FILEID,
        volattr: 0,
        dirattr: 0,
        fileattr: libc::ATTR_FILE_LINKCOUNT | libc::ATTR_FILE_ALLOCSIZE,
        forkattr: 0,
    };

    // Prefix (with trailing separator) shared by every child path
    let mut prefix = String::with_capacity(path.len() + 1);
    prefix.push_str(path);
    if !prefix.ends_with('/') {
        prefix.push('/');
    }

    let mut blocks: i64 = 0;
    let mut subdirs = Vec::new();

    loop {
        let retcount = unsafe {
            libc::getattrlistbulk(
                dirfd,
                &mut attrlist as *mut libc::attrlist as *mut libc::c_void,
                attrbuf.as_mut_ptr() as *mut libc::c_void,
                attrbuf.len(),
                0,
            )
        };

        if retcount <= 0 {
            if retcount < 0 {
                let errno = unsafe { *libc::__error() };
                let error_msg = match errno {
                    libc::EACCES => "Permission denied",
                    libc::ENOENT => "No such file or directory",
                    _ => "Cannot read directory contents",
                };
                return Err(format!("{}: {}", path, error_msg));
            }
            break;
        }

        // Parse attribute buffer
        let mut entry_ptr = attrbuf.as_ptr();
        for _ in 0..retcount {
            unsafe {
                // Read entry length and advance to attribute data
                let entry_length = std::ptr::read_unaligned(entry_ptr as *const u32);
                let mut field_ptr = entry_ptr.add(std::mem::size_of::<u32>());

                // Read returned attributes bitmask
                let returned_attrs =
                    std::ptr::read_unaligned(field_ptr as *const libc::attribute_set_t);
                field_ptr = field_ptr.add(std::mem::size_of::<libc::attribute_set_t>());

                // Extract filename (borrowed from the buffer, no allocation)
                let mut filename: Option<&str> = None;
                if returned_attrs.commonattr & libc::ATTR_CMN_NAME != 0 {
                    let name_start = field_ptr; // Save start of attrreference_t
                    let name_info =
                        std::ptr::read_unaligned(field_ptr as *const libc::attrreference_t);
                    field_ptr = field_ptr.add(std::mem::size_of::<libc::attrreference_t>());
                    let name_ptr = name_start.add(name_info.attr_dataoffset as usize);

                    if name_info.attr_length > 0 {
                        let name_slice = std::slice::from_raw_parts(
                            name_ptr,
                            (name_info.attr_length - 1) as usize,
                        );
                        if let Ok(name_str) = std::str::from_utf8(name_slice) {
                            if is_dot_or_dotdot(name_str) {
                                entry_ptr = entry_ptr.add(entry_length as usize);
                                continue;
                            }
                            filename = Some(name_str);
                        }
                    }
                }

                // Check for errors
                if returned_attrs.commonattr & ATTR_CMN_ERROR != 0 {
                    let error_code = std::ptr::read_unaligned(field_ptr as *const u32);
                    field_ptr = field_ptr.add(std::mem::size_of::<u32>());
                    if error_code != 0 {
                        if let Some(name) = filename {
                            eprintln!("cannot access '{}{}': error {}", prefix, name, error_code);
                        }
                        entry_ptr = entry_ptr.add(entry_length as usize);
                        continue;
                    }
                }

                // Get object type
                let obj_type = if returned_attrs.commonattr & libc::ATTR_CMN_OBJTYPE != 0 {
                    let obj_type = std::ptr::read_unaligned(field_ptr as *const u32);
                    field_ptr = field_ptr.add(std::mem::size_of::<u32>());
                    obj_type
                } else {
                    VNON
                };

                // Get inode
                let inode = if returned_attrs.commonattr & libc::ATTR_CMN_FILEID != 0 {
                    let inode = std::ptr::read_unaligned(field_ptr as *const u64);
                    field_ptr = field_ptr.add(std::mem::size_of::<u64>());
                    inode
                } else {
                    0
                };

                // Link count comes first among the file attributes. Only files with
                // more than one link can be duplicates, so only they touch the inode set.
                let mut nlink: u32 = 2; // assume shared if the kernel didn't say
                if obj_type == VREG || obj_type == VLNK {
                    if returned_attrs.fileattr & libc::ATTR_FILE_LINKCOUNT != 0 {
                        nlink = std::ptr::read_unaligned(field_ptr as *const u32);
                        field_ptr = field_ptr.add(std::mem::size_of::<u32>());
                    }
                }

                // Handle different file types
                match obj_type {
                    VREG => {
                        // Regular file - get allocation size
                        if returned_attrs.fileattr & libc::ATTR_FILE_ALLOCSIZE != 0 {
                            let alloc_size = std::ptr::read_unaligned(field_ptr as *const i64);
                            let file_blocks = blocks_from_bytes(alloc_size);
                            blocks += if nlink > 1 {
                                check_and_add_inode(inode, file_blocks)
                            } else {
                                file_blocks
                            };
                        }
                    }
                    VDIR => {
                        // Directory - record its full path
                        if let Some(name) = filename {
                            let mut child = String::with_capacity(prefix.len() + name.len());
                            child.push_str(&prefix);
                            child.push_str(name);
                            subdirs.push(child);
                        }
                    }
                    VLNK => {
                        // Symlink - count the link itself as 1 (du default behavior)
                        blocks += if nlink > 1 {
                            check_and_add_inode(inode, 1)
                        } else {
                            1
                        };
                    }
                    _ => {
                        // Other file types (devices, etc.) - treat as zero-size
                    }
                }

                // Move to next entry
                entry_ptr = entry_ptr.add(entry_length as usize);
            }
        }
    }

    Ok(DirInfo { blocks, subdirs })
}
