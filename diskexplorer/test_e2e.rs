use std::fs;
use std::path::Path;
use trash;

fn main() {
    let temp = std::env::temp_dir();
    let test_file = temp.join("test_e2e_trash_debug.txt");
    fs::write(&test_file, "test content for e2e trash test").unwrap();
    
    println!("Test file path: {}", test_file.display());
    
    // Try to trash
    let result = trash::delete(&test_file);
    println!("trash::delete result: {:?}", result);
    
    // Check if file exists
    println!("File exists after: {}", test_file.exists());
    
    // Check os_limited::list()
    let items = trash::os_limited::list().unwrap();
    println!("Items in recycle bin: {}", items.len());
    for item in items {
        println!("  Original path: {}", item.original_path().display());
        println!("  Path: {}", item.path().display());
        if item.original_path() == test_file {
            println!("  *** MATCH FOUND ***");
        }
    }
}
