use std::collections::HashMap;

fn main() {
    println!("cargo:rerun-if-changed=secrets.env");

    // Load secrets.env (KEY=VALUE lines, # comments ignored)
    let vars: HashMap<String, String> = std::fs::read_to_string("secrets.env")
        .unwrap_or_default()
        .lines()
        .filter(|l| !l.trim_start().starts_with('#') && l.contains('='))
        .filter_map(|l| {
            let (k, v) = l.split_once('=')?;
            Some((k.trim().to_string(), v.trim().to_string()))
        })
        .collect();

    for var in ["WIFI_SSID", "WIFI_PASSWORD", "THINGSBOARD_TOKEN"] {
        println!(
            "cargo:rustc-env={var}={}",
            vars.get(var).map(String::as_str).unwrap_or_default()
        );
    }

    embuild::espidf::sysenv::output();
}
