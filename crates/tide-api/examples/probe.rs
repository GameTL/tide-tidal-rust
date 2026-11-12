use serde_json::Value;
use tide_api::TidalService;

fn shape(v: &Value, depth: usize, out: &mut String, indent: usize) {
    let pad = " ".repeat(indent);
    match v {
        Value::Object(m) => {
            for (k, x) in m {
                match x {
                    Value::Object(_) if depth > 0 => { out.push_str(&format!("{pad}{k}:\n")); shape(x, depth - 1, out, indent + 2); }
                    Value::Array(a) => {
                        out.push_str(&format!("{pad}{k}: [{}]\n", a.len()));
                        if depth > 0 && let Some(f) = a.first() { shape(f, depth - 1, out, indent + 2); }
                    }
                    Value::String(s) => out.push_str(&format!("{pad}{k}: \"{}\"\n", s.chars().take(50).collect::<String>())),
                    other => out.push_str(&format!("{pad}{k}: {}\n", other.to_string().chars().take(50).collect::<String>())),
                }
            }
        }
        other => out.push_str(&format!("{pad}{}\n", other.to_string().chars().take(60).collect::<String>())),
    }
}

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().collect();
    let depth: usize = args.get(2).and_then(|d| d.parse().ok()).unwrap_or(3);
    let mut svc = TidalService::with_device_auth().unwrap();
    assert!(svc.load_session().await.unwrap(), "not logged in");
    let api = svc.handle().unwrap();
    let uid = api.user_id().to_string();
    let url = args[1].replace("{uid}", &uid);
    let (base, query) = url.split_once('?').unwrap_or((&url, ""));
    let params: Vec<(&str, &str)> = query.split('&').filter(|s| !s.is_empty()).filter_map(|kv| kv.split_once('=')).collect();
    match api.get(base, &params).await {
        Ok(v) => { if std::env::var("RAW").is_ok() { println!("{v}"); return; } let mut o = String::new(); shape(&v, depth, &mut o, 0); println!("{o}"); }
        Err(e) => println!("ERR {e}"),
    }
}
