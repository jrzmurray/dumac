use std::fs::{self, File, hard_link};
use std::io::Write;
use std::os::unix::fs::MetadataExt;
use tempfile::TempDir;

// Import the main module
#[path = "../src/main.rs"]
mod main;

use main::calculate_size;

#[test]
fn test_basic_file_size_calculation() {
    // Clear the seen inodes cache to ensure test isolation
    main::clear_seen_inodes();
    
    // Create a temporary directory
    let temp_dir = TempDir::new().expect("Failed to create temp dir");
    let temp_path = temp_dir.path();
    
    // Create a file with known content
    let file_path = temp_path.join("test_file.txt");
    let mut file = File::create(&file_path).expect("Failed to create test file");
    
    // Write 1000 bytes
    let content = "a".repeat(1000);
    file.write_all(content.as_bytes()).expect("Failed to write to file");
    file.sync_all().expect("Failed to sync file");
    drop(file);
    
    // Calculate size
    let result = calculate_size(temp_path.to_string_lossy().to_string());
    assert!(result.is_ok(), "calculate_size should succeed");
    
    let total_blocks = result.unwrap();
    
    // 1000 bytes should be at least 2 blocks (1000 + 511) / 512 = 2 blocks
    // But filesystem allocation might be larger
    assert!(total_blocks >= 2, "Should have at least 2 blocks for 1000 bytes, got {}", total_blocks);
    
    // Cleanup happens automatically when TempDir is dropped
}

#[test]
fn test_nested_directories() {
    // Clear the seen inodes cache to ensure test isolation
    main::clear_seen_inodes();
    
    let temp_dir = TempDir::new().expect("Failed to create temp dir");
    let temp_path = temp_dir.path();
    
    // Create nested directory structure
    let subdir = temp_path.join("subdir");
    fs::create_dir(&subdir).expect("Failed to create subdir");
    
    // Create files in both root and subdir
    let root_file = temp_path.join("root.txt");
    let mut file1 = File::create(&root_file).expect("Failed to create root file");
    file1.write_all(b"hello").expect("Failed to write to root file");
    drop(file1);
    
    let sub_file = subdir.join("sub.txt");
    let mut file2 = File::create(&sub_file).expect("Failed to create sub file");
    file2.write_all(b"world").expect("Failed to write to sub file");
    drop(file2);
    
    // Calculate total size
    let result = calculate_size(temp_path.to_string_lossy().to_string());
    assert!(result.is_ok(), "calculate_size should succeed for nested dirs");
    
    let total_blocks = result.unwrap();
    // Should have blocks for both files (minimum 2 blocks total)
    assert!(total_blocks >= 2, "Should have at least 2 blocks for two files, got {}", total_blocks);
}

#[test]
fn test_hardlink_deduplication() {
    // Clear the seen inodes cache to ensure test isolation
    main::clear_seen_inodes();
    
    let temp_dir = TempDir::new().expect("Failed to create temp dir");
    let temp_path = temp_dir.path();
    
    // Create original file with substantial content
    let original_file = temp_path.join("original.txt");
    let mut file = File::create(&original_file).expect("Failed to create original file");
    
    // Write 2048 bytes (should be 4 blocks: (2048 + 511) / 512 = 4)
    let content = "x".repeat(2048);
    file.write_all(content.as_bytes()).expect("Failed to write to original file");
    file.sync_all().expect("Failed to sync original file");
    drop(file);
    
    // Calculate size with just the original file
    let size_original = calculate_size(temp_path.to_string_lossy().to_string())
        .expect("Failed to calculate size for original");
    
    // Create hard link to the same file
    let hardlink_file = temp_path.join("hardlink.txt");
    hard_link(&original_file, &hardlink_file).expect("Failed to create hard link");
    
    // Verify the hardlink was created successfully
    let original_metadata = fs::metadata(&original_file).expect("Failed to get original metadata");
    let hardlink_metadata = fs::metadata(&hardlink_file).expect("Failed to get hardlink metadata");
    assert_eq!(original_metadata.ino(), hardlink_metadata.ino(), "Hardlink should have same inode");
    
    // Clear cache again before second calculation to test deduplication logic
    main::clear_seen_inodes();
    
    // Calculate size again - should be the same due to deduplication
    let size_with_hardlink = calculate_size(temp_path.to_string_lossy().to_string())
        .expect("Failed to calculate size with hardlink");
    
    // The total size should be the same because hardlinks should be deduplicated
    assert_eq!(
        size_original, 
        size_with_hardlink,
        "Hardlinked files should not double-count blocks. Original: {}, With hardlink: {}",
        size_original,
        size_with_hardlink
    );
    
    // Verify the original size is reasonable (at least 4 blocks for 2048 bytes)
    assert!(size_original >= 4, "Should have at least 4 blocks for 2048 bytes, got {}", size_original);
} 

use main::{expand_args, size_of_path};
use std::process::Command;

fn write_file(path: &std::path::Path, bytes: usize) {
    let mut f = File::create(path).expect("Failed to create file");
    f.write_all(&vec![b'a'; bytes]).expect("Failed to write file");
}

#[test]
fn test_expand_args_glob_matches() {
    let temp_dir = TempDir::new().unwrap();
    let root = temp_dir.path();
    fs::create_dir(root.join("sub1")).unwrap();
    fs::create_dir(root.join("sub2")).unwrap();
    fs::create_dir(root.join("other")).unwrap();
    fs::create_dir(root.join(".hidden")).unwrap();

    let pattern = format!("{}/sub*", root.display());
    let (paths, errors) = expand_args(&[pattern]);
    assert!(errors.is_empty(), "unexpected errors: {:?}", errors);
    assert_eq!(
        paths,
        vec![
            format!("{}/sub1", root.display()),
            format!("{}/sub2", root.display())
        ]
    );

    // Leading dots are not matched by a bare wildcard, like a shell
    let (paths, _) = expand_args(&[format!("{}/*", root.display())]);
    assert!(paths.iter().all(|p| !p.contains(".hidden")));
}

#[test]
fn test_expand_args_recursive_glob() {
    let temp_dir = TempDir::new().unwrap();
    let root = temp_dir.path();
    fs::create_dir_all(root.join("a/build")).unwrap();
    fs::create_dir_all(root.join("b/c/build")).unwrap();

    let (paths, errors) = expand_args(&[format!("{}/**/build", root.display())]);
    assert!(errors.is_empty());
    assert_eq!(paths.len(), 2, "got {:?}", paths);
}

#[test]
fn test_expand_args_no_match_and_literal() {
    let temp_dir = TempDir::new().unwrap();
    let root = temp_dir.path();

    let (paths, errors) = expand_args(&[format!("{}/nothing*", root.display())]);
    assert!(paths.is_empty());
    assert_eq!(errors.len(), 1);
    assert!(errors[0].contains("No such file or directory"));

    // A literal path that happens to contain glob characters is not expanded
    let weird = root.join("we[ir]d*");
    fs::create_dir(&weird).unwrap();
    let arg = weird.to_string_lossy().to_string();
    let (paths, errors) = expand_args(&[arg.clone()]);
    assert!(errors.is_empty());
    assert_eq!(paths, vec![arg]);

    // Plain nonexistent paths pass through so the size step reports the error
    let (paths, errors) = expand_args(&["/definitely/not/here".to_string()]);
    assert!(errors.is_empty());
    assert_eq!(paths.len(), 1);
}

#[test]
fn test_size_of_path_file_root() {
    main::clear_seen_inodes();
    let temp_dir = TempDir::new().unwrap();
    let file = temp_dir.path().join("f.txt");
    write_file(&file, 4096);

    let blocks = size_of_path(file.to_string_lossy().to_string()).unwrap();
    assert!(blocks >= 8, "4096 bytes should be at least 8 blocks, got {}", blocks);
}

#[test]
fn test_hardlink_shared_across_roots_counted_once() {
    main::clear_seen_inodes();
    let temp_dir = TempDir::new().unwrap();
    let root = temp_dir.path();
    fs::create_dir(root.join("a")).unwrap();
    fs::create_dir(root.join("b")).unwrap();
    write_file(&root.join("a/file"), 8192);
    hard_link(root.join("a/file"), root.join("b/link")).unwrap();

    let first = size_of_path(root.join("a").to_string_lossy().to_string()).unwrap();
    let second = size_of_path(root.join("b").to_string_lossy().to_string()).unwrap();
    assert!(first >= 16);
    assert_eq!(second, 0, "hardlink already counted in the first root");
}

#[test]
fn test_cli_multiple_paths_and_quoted_glob() {
    let temp_dir = TempDir::new().unwrap();
    let root = temp_dir.path();
    fs::create_dir(root.join("sub1")).unwrap();
    fs::create_dir(root.join("sub2")).unwrap();
    write_file(&root.join("sub1/x"), 10_000);
    write_file(&root.join("sub2/y"), 10_000);

    let pattern = format!("{}/sub*", root.display());
    let out = Command::new(env!("CARGO_BIN_EXE_dumac"))
        .arg(&pattern)
        .output()
        .unwrap();
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert_eq!(stdout.lines().count(), 2, "got {:?}", stdout);
    assert!(stdout.contains("sub1") && stdout.contains("sub2"));

    let out = Command::new(env!("CARGO_BIN_EXE_dumac"))
        .arg(root.join("sub1"))
        .arg(format!("{}/nomatch*", root.display()))
        .output()
        .unwrap();
    assert!(!out.status.success(), "no-match pattern should fail");
    assert_eq!(String::from_utf8_lossy(&out.stdout).lines().count(), 1);
}

#[test]
fn test_symlinks_report_allocated_size() {
    main::clear_seen_inodes();

    let temp_dir = TempDir::new().expect("Failed to create temp dir");
    let temp_path = temp_dir.path();

    let links: Vec<_> = (0..100)
        .map(|i| {
            let link = temp_path.join(format!("link_{}", i));
            std::os::unix::fs::symlink(format!("/some/target/path/file_{}", i), &link)
                .expect("Failed to create symlink");
            link
        })
        .collect();

    // du counts a symlink's own st_blocks, which is 0 where the filesystem stores
    // short targets inline. Derive the expectation so this holds off APFS too.
    let expected_blocks: i64 = links
        .iter()
        .map(|link| {
            fs::symlink_metadata(link)
                .expect("Failed to stat symlink")
                .blocks() as i64
        })
        .sum();

    let total_blocks = calculate_size(temp_path.to_string_lossy().to_string())
        .expect("calculate_size should succeed");

    assert_eq!(
        total_blocks, expected_blocks,
        "symlinks should contribute their real allocated size, not a fixed block each"
    );
}
