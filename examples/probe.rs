fn main(){
    let status: serde_json::Value = serde_json::from_str(include_str!("/tmp/st.json")).unwrap();
    let s = dg::views::service_dashboard(&status);
    for sec in &s { println!("{} blocks={}", sec.title(), sec.blocks().len()); }
}
