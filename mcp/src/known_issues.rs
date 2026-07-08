//! Known-issues lookup: decode a vehicle and pull real recall / complaint data
//! from NHTSA so the MCP can answer "what's known to go wrong on this vehicle"
//! from public data instead of the model's general knowledge.
//!
//! Data source: NHTSA public APIs (US DOT, public domain, no API key):
//!   - vPIC        VIN -> make/model/year   (vpic.nhtsa.dot.gov)
//!   - recalls     recallsByVehicle          (api.nhtsa.gov)
//!   - complaints  complaintsByVehicle       (api.nhtsa.gov)
//!
//! The HTTP transport is abstracted behind [`HttpClient`] so it can be mocked in
//! tests (keeping them offline and fast) and swapped for a real client. The
//! default client shells out to `curl`; a production build would use a proper
//! Rust HTTP client (ureq/reqwest). Responses are cached per URL so a repeated
//! lookup works without re-hitting the network.

use std::collections::HashMap;

use serde_json::{json, Value};

/// Minimal HTTP GET returning parsed JSON. Abstracted for testability.
pub trait HttpClient {
    fn get_json(&self, url: &str) -> Result<Value, String>;
}

/// Default transport: shell out to `curl`. Keeps the crate dependency-free for
/// this prototype; replace with ureq/reqwest to productionize.
pub struct CurlClient;

impl HttpClient for CurlClient {
    fn get_json(&self, url: &str) -> Result<Value, String> {
        let out = std::process::Command::new("curl")
            .args([
                "-sS",
                "--max-time",
                "15",
                "-H",
                "Accept: application/json",
                url,
            ])
            .output()
            .map_err(|e| format!("failed to run curl (is it installed?): {e}"))?;
        if !out.status.success() {
            return Err(format!(
                "network request failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }
        serde_json::from_slice(&out.stdout)
            .map_err(|e| format!("invalid JSON from {url}: {e}"))
    }
}

pub struct Vehicle {
    pub make: String,
    pub model: String,
    pub year: i64,
    pub cylinders: Option<i64>,
    pub displacement_l: Option<f64>,
    pub fuel_type: Option<String>,
    pub drive_type: Option<String>,
    pub body_class: Option<String>,
    pub engine_model: Option<String>,
}

impl Vehicle {
    /// A vehicle known only by make/model/year (e.g. supplied directly, without
    /// a VIN to decode the richer attributes from).
    pub fn basic(make: impl Into<String>, model: impl Into<String>, year: i64) -> Self {
        Self {
            make: make.into(),
            model: model.into(),
            year,
            cylinders: None,
            displacement_l: None,
            fuel_type: None,
            drive_type: None,
            body_class: None,
            engine_model: None,
        }
    }

    fn to_json(&self) -> Value {
        let mut v = json!({"make": self.make, "model": self.model, "year": self.year});
        let obj = v.as_object_mut().unwrap();
        if let Some(c) = self.cylinders {
            obj.insert("cylinders".into(), json!(c));
            // Heuristic bank hint: cylinders alone can't distinguish inline from
            // V, so 6 is genuinely ambiguous.
            let hint = if c >= 8 {
                "2"
            } else if c <= 4 {
                "1"
            } else {
                "1 (inline) or 2 (V) — check engine layout"
            };
            obj.insert("bank_hint".into(), json!(hint));
        }
        if let Some(d) = self.displacement_l {
            obj.insert("displacement_l".into(), json!(d));
        }
        if let Some(f) = &self.fuel_type {
            obj.insert("fuel_type".into(), json!(f));
        }
        if let Some(d) = &self.drive_type {
            obj.insert("drive_type".into(), json!(d));
        }
        if let Some(b) = &self.body_class {
            obj.insert("body_class".into(), json!(b));
        }
        if let Some(e) = &self.engine_model {
            obj.insert("engine_model".into(), json!(e));
        }
        v
    }
}

pub struct KnownIssues {
    http: Box<dyn HttpClient>,
    cache: HashMap<String, Value>,
}

impl Default for KnownIssues {
    fn default() -> Self {
        Self::with_client(Box::new(CurlClient))
    }
}

impl KnownIssues {
    pub fn with_client(http: Box<dyn HttpClient>) -> Self {
        Self {
            http,
            cache: HashMap::new(),
        }
    }

    /// GET with a per-URL cache. Returns (json, from_cache).
    fn get_cached(&mut self, url: &str) -> Result<(Value, bool), String> {
        if let Some(v) = self.cache.get(url) {
            return Ok((v.clone(), true));
        }
        let v = self.http.get_json(url)?;
        self.cache.insert(url.to_string(), v.clone());
        Ok((v, false))
    }

    /// Decode a VIN to make/model/year via NHTSA vPIC.
    pub fn decode_vin(&mut self, vin: &str) -> Result<Vehicle, String> {
        let url = format!(
            "https://vpic.nhtsa.dot.gov/api/vehicles/DecodeVinValues/{}?format=json",
            enc(vin)
        );
        let (body, _) = self.get_cached(&url)?;
        let r = body
            .get("Results")
            .and_then(|r| r.get(0))
            .ok_or("vPIC returned no results for that VIN")?;

        let make = str_field(r, &["Make"]).unwrap_or_default();
        let model = str_field(r, &["Model"]).unwrap_or_default();
        let year = str_field(r, &["ModelYear"])
            .and_then(|y| y.parse::<i64>().ok())
            .unwrap_or(0);

        if make.is_empty() || model.is_empty() || year == 0 {
            return Err(format!(
                "vPIC could not fully decode VIN `{vin}` (make/model/year incomplete)"
            ));
        }

        Ok(Vehicle {
            make,
            model,
            year,
            cylinders: str_field(r, &["EngineCylinders"]).and_then(|s| s.parse().ok()),
            displacement_l: str_field(r, &["DisplacementL"]).and_then(|s| s.parse().ok()),
            fuel_type: str_field(r, &["FuelTypePrimary"]),
            drive_type: str_field(r, &["DriveType"]),
            body_class: str_field(r, &["BodyClass"]),
            engine_model: str_field(r, &["EngineModel"]),
        })
    }

    /// Fetch and summarize recalls + complaints for a vehicle. `component` is an
    /// optional case-insensitive substring filter (e.g. "engine", "fuel").
    pub fn lookup(&mut self, vehicle: &Vehicle, component: Option<&str>) -> Result<Value, String> {
        let recalls_url = format!(
            "https://api.nhtsa.gov/recalls/recallsByVehicle?make={}&model={}&modelYear={}",
            enc(&vehicle.make),
            enc(&vehicle.model),
            vehicle.year
        );
        let complaints_url = format!(
            "https://api.nhtsa.gov/complaints/complaintsByVehicle?make={}&model={}&modelYear={}",
            enc(&vehicle.make),
            enc(&vehicle.model),
            vehicle.year
        );

        let (recalls_body, r_cached) = self.get_cached(&recalls_url)?;
        let (complaints_body, c_cached) = self.get_cached(&complaints_url)?;

        let recalls = summarize_recalls(&recalls_body, component);
        let complaints = summarize_complaints(&complaints_body, component);

        Ok(json!({
            "vehicle": vehicle.to_json(),
            "recalls": recalls,
            "complaints": complaints,
            "filter": component,
            "source": "NHTSA (api.nhtsa.gov / vpic.nhtsa.dot.gov), public domain",
            "from_cache": r_cached && c_cached,
        }))
    }
}

/// NHTSA recalls: `{ Count, results: [ { Component, Summary, Remedy, Consequence,
/// NHTSACampaignNumber, ReportReceivedDate } ] }` (field casing varies).
fn summarize_recalls(body: &Value, component: Option<&str>) -> Value {
    let results = array_field(body, &["results", "Results"]);
    let items: Vec<Value> = results
        .iter()
        .filter(|it| matches_component(str_field(it, &["Component", "component"]), component))
        .take(10)
        .map(|it| {
            json!({
                "campaign": str_field(it, &["NHTSACampaignNumber", "campaignNumber"]),
                "component": str_field(it, &["Component", "component"]),
                "summary": str_field(it, &["Summary", "summary"]),
                "remedy": str_field(it, &["Remedy", "remedy"]),
                "reported": str_field(it, &["ReportReceivedDate", "reportReceivedDate"]),
            })
        })
        .collect();
    json!({"count": items.len(), "items": items})
}

/// NHTSA complaints: `{ count, results: [ { components, summary, crash, fire,
/// dateComplaintFiled } ] }`. We also group by component to surface the most
/// complained-about systems.
fn summarize_complaints(body: &Value, component: Option<&str>) -> Value {
    let results = array_field(body, &["results", "Results"]);

    // Component frequency across all (filtered) complaints.
    let mut counts: HashMap<String, usize> = HashMap::new();
    let mut filtered: Vec<&Value> = Vec::new();
    for it in &results {
        let comp = str_field(it, &["components", "Components", "component"]).unwrap_or_default();
        if !matches_component(Some(comp.clone()), component) {
            continue;
        }
        for part in comp.split([',', '|']) {
            let key = part.trim().to_uppercase();
            if !key.is_empty() {
                *counts.entry(key).or_insert(0) += 1;
            }
        }
        filtered.push(it);
    }

    let mut top: Vec<(String, usize)> = counts.into_iter().collect();
    top.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    let top_components: Vec<Value> = top
        .into_iter()
        .take(8)
        .map(|(k, v)| json!({"component": k, "count": v}))
        .collect();

    let items: Vec<Value> = filtered
        .iter()
        .take(10)
        .map(|it| {
            json!({
                "component": str_field(it, &["components", "Components", "component"]),
                "summary": str_field(it, &["summary", "Summary"]),
                "filed": str_field(it, &["dateComplaintFiled", "dateFiled"]),
                "crash": it.get("crash").and_then(Value::as_bool),
                "fire": it.get("fire").and_then(Value::as_bool),
            })
        })
        .collect();

    json!({
        "count": filtered.len(),
        "top_components": top_components,
        "items": items,
    })
}

fn matches_component(value: Option<String>, filter: Option<&str>) -> bool {
    match filter {
        None => true,
        Some(f) => value
            .map(|v| v.to_lowercase().contains(&f.to_lowercase()))
            .unwrap_or(false),
    }
}

/// Read a string field, trying several key spellings; empty/absent -> None.
fn str_field(obj: &Value, keys: &[&str]) -> Option<String> {
    for k in keys {
        if let Some(s) = obj.get(*k).and_then(Value::as_str) {
            if !s.trim().is_empty() {
                return Some(s.trim().to_string());
            }
        }
    }
    None
}

fn array_field(obj: &Value, keys: &[&str]) -> Vec<Value> {
    for k in keys {
        if let Some(a) = obj.get(*k).and_then(Value::as_array) {
            return a.clone();
        }
    }
    Vec::new()
}

/// Minimal URL encoding for the pieces we insert (spaces and a few specials).
fn enc(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    /// Mock transport: returns canned JSON based on a URL substring and counts
    /// how many times each was actually fetched (to prove caching).
    struct MockHttp {
        calls: RefCell<Vec<String>>,
    }

    impl MockHttp {
        fn new() -> Self {
            Self {
                calls: RefCell::new(Vec::new()),
            }
        }
    }

    impl HttpClient for MockHttp {
        fn get_json(&self, url: &str) -> Result<Value, String> {
            self.calls.borrow_mut().push(url.to_string());
            if url.contains("DecodeVinValues") {
                Ok(json!({"Results": [{
                    "Make": "TOYOTA", "Model": "4Runner", "ModelYear": "2013",
                    "EngineCylinders": "6", "DisplacementL": "4.0",
                    "FuelTypePrimary": "Gasoline", "DriveType": "4WD",
                    "BodyClass": "Sport Utility Vehicle (SUV)/Multi-Purpose Vehicle (MPV)",
                    "EngineModel": "1GR-FE"
                }]}))
            } else if url.contains("recallsByVehicle") {
                Ok(json!({"Count": 1, "results": [{
                    "NHTSACampaignNumber": "13V001000",
                    "Component": "FUEL SYSTEM, GASOLINE",
                    "Summary": "Fuel delivery pipe may leak.",
                    "Remedy": "Dealers will replace the pipe.",
                    "ReportReceivedDate": "2013-01-15"
                }]}))
            } else if url.contains("complaintsByVehicle") {
                Ok(json!({"count": 3, "results": [
                    {"components": "ENGINE", "summary": "Rough idle and misfire.",
                     "dateComplaintFiled": "2015-03-01", "crash": false, "fire": false},
                    {"components": "ENGINE", "summary": "Check engine light, lean code.",
                     "dateComplaintFiled": "2016-07-04", "crash": false, "fire": false},
                    {"components": "ELECTRICAL SYSTEM", "summary": "Battery drain.",
                     "dateComplaintFiled": "2017-02-02", "crash": false, "fire": false}
                ]}))
            } else {
                Err(format!("unexpected url {url}"))
            }
        }
    }

    fn ki() -> KnownIssues {
        KnownIssues::with_client(Box::new(MockHttp::new()))
    }

    #[test]
    fn decodes_vin_with_richer_attributes() {
        let mut k = ki();
        let v = k.decode_vin("JTEBU5JR0D5123456").unwrap();
        assert_eq!(v.make, "TOYOTA");
        assert_eq!(v.model, "4Runner");
        assert_eq!(v.year, 2013);
        assert_eq!(v.cylinders, Some(6));
        assert_eq!(v.displacement_l, Some(4.0));
        assert_eq!(v.fuel_type.as_deref(), Some("Gasoline"));
        assert_eq!(v.engine_model.as_deref(), Some("1GR-FE"));
    }

    #[test]
    fn lookup_surfaces_vehicle_attributes_from_a_decoded_vin() {
        let mut k = ki();
        let v = k.decode_vin("JTEBU5JR0D5123456").unwrap();
        let out = k.lookup(&v, None).unwrap();
        assert_eq!(out["vehicle"]["cylinders"], json!(6));
        assert_eq!(out["vehicle"]["displacement_l"], json!(4.0));
        assert_eq!(out["vehicle"]["engine_model"], json!("1GR-FE"));
        // 6 cylinders is ambiguous between inline and V.
        assert!(out["vehicle"]["bank_hint"].as_str().unwrap().contains("V"));
    }

    #[test]
    fn lookup_aggregates_recalls_and_complaints() {
        let mut k = ki();
        let out = k.lookup(&Vehicle::basic("TOYOTA", "4Runner", 2013), None).unwrap();
        assert_eq!(out["recalls"]["count"], json!(1));
        assert_eq!(out["recalls"]["items"][0]["campaign"], json!("13V001000"));
        assert_eq!(out["complaints"]["count"], json!(3));
        // ENGINE is the most-complained component (2 of 3).
        assert_eq!(out["complaints"]["top_components"][0]["component"], json!("ENGINE"));
        assert_eq!(out["complaints"]["top_components"][0]["count"], json!(2));
    }

    #[test]
    fn component_filter_narrows_complaints() {
        let mut k = ki();
        let out = k
            .lookup(&Vehicle::basic("TOYOTA", "4Runner", 2013), Some("engine"))
            .unwrap();
        // Only the two ENGINE complaints survive; the electrical one is filtered.
        assert_eq!(out["complaints"]["count"], json!(2));
        assert_eq!(out["filter"], json!("engine"));
    }

    #[test]
    fn responses_are_cached_per_url() {
        let mut k = KnownIssues::with_client(Box::new(MockHttp::new()));
        let _ = k.lookup(&Vehicle::basic("TOYOTA", "4Runner", 2013), None).unwrap();
        let second = k.lookup(&Vehicle::basic("TOYOTA", "4Runner", 2013), None).unwrap();
        assert_eq!(second["from_cache"], json!(true));
        // A fresh lookup of a *different* year would not be cached.
        let third = k.lookup(&Vehicle::basic("TOYOTA", "4Runner", 2014), None).unwrap();
        assert_eq!(third["from_cache"], json!(false));
    }

    #[test]
    fn url_encoding_handles_spaces() {
        assert_eq!(enc("Grand Cherokee"), "Grand%20Cherokee");
        assert_eq!(enc("4Runner"), "4Runner");
    }
}
