use trash;

fn main() {
    let items = trash::os_limited::list().unwrap();
    for item in items {
        println!("{}", item.path().display());
    }
}
