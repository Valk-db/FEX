// test trash::os_limited::list()
use trash;

fn main() {
    let items = trash::os_limited::list().unwrap();
    for item in items {
        println!("Path: {}", item.path().display());
        println!("Original path: {}", item.original_path().display());
        println!("Deletion time: {:?}", item.deletion_time());
        println!("---");
    }
}
