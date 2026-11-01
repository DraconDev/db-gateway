fn main() {
    for s in ["http://127.0.0.1:1", "http://127.0.0.1:1/", "http://127.0.0.1:9/", "http://localhost:1/"] {
        match reqwest::Url::parse(s) {
            Ok(u) => println!("{s} -> path={:?} host={:?}", u.path(), u.host_str()),
            Err(e) => println!("{s} -> ERR {e}"),
        }
    }
}
