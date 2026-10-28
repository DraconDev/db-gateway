fn main() {
    let structure: serde_json::Value = serde_json::json!({
        "database":"inventory","tombstoned":false,"created_at":1,
        "tables":[{"name":"products","rows":60,"columns":[
            {"name":"id","type":"INTEGER","not_null":false,"primary_key":true,"default":null},
            {"name":"sku","type":"TEXT","not_null":true,"primary_key":false,"default":null}],
          "indexes":[],"foreign_keys":[],"sql":"CREATE TABLE products (id INTEGER PRIMARY KEY, sku TEXT)"}]
    });
    for filter in [
        "sku = 'a'' OR 1=1 union select 1,2,3,4,5,6,7 where ''='",
        "1=1 AND sku='x'' UNION SELECT 1,2,3,4,5,6,7 FROM products WHERE ''='",
        "1=1) union select 1",
    ] {
        match dg::views::rows_query(&structure, "products", 0, None, Some(filter)) {
            Ok(q) => println!("ACCEPTED filter={filter:?}\n  SQL= {}", q.sql),
            Err(e) => println!("REFUSED filter={filter:?} -> {e:?}"),
        }
    }
}
