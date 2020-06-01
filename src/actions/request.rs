use std::collections::HashMap;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use colored::*;
use reqwest::{
  header::{self, HeaderMap, HeaderName, HeaderValue},
  ClientBuilder, Method, Response,
};
use url::Url;
use yaml_rust::Yaml;

use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};

use crate::actions::{extract, extract_optional};
use crate::benchmark::{Context, Pool, Reports};
use crate::config::Config;
use crate::interpolator;

use crate::actions::{Report, Runnable};

static USER_AGENT: &str = "drill";

#[derive(Clone)]
pub struct Request {
  name: String,
  url: String,
  time: f64,
  method: String,
  headers: HashMap<String, String>,
  pub body: Option<String>,
  pub with_item: Option<Yaml>,
  pub assign: Option<String>,
}

#[derive(Serialize, Deserialize)]
struct AssignedRequest {
  body: Value,
  headers: Map<String, Value>,
}

impl Request {
  pub fn is_that_you(item: &Yaml) -> bool {
    item["request"].as_hash().is_some()
  }

  pub fn new(item: &Yaml, with_item: Option<Yaml>) -> Request {
    let name = extract(item, "name");
    let url = extract(&item["request"], "url");
    let assign = extract_optional(item, "assign");

    let method = if let Some(v) = extract_optional(&item["request"], "method") {
      v.to_string().to_uppercase()
    } else {
      "GET".to_string()
    };

    let body = if method == "POST" {
      Some(extract(&item["request"], "body"))
    } else {
      None
    };

    let mut headers = HashMap::new();

    if let Some(hash) = item["request"]["headers"].as_hash() {
      for (key, val) in hash.iter() {
        if let Some(vs) = val.as_str() {
          headers.insert(key.as_str().unwrap().to_string(), vs.to_string());
        } else {
          panic!("{} Headers must be strings!!", "WARNING!".yellow().bold());
        }
      }
    }

    Request {
      name: name.to_string(),
      url: url.to_string(),
      time: 0.0,
      method,
      headers,
      body: body.map(str::to_string),
      with_item,
      assign: assign.map(str::to_string),
    }
  }

  fn format_time(tdiff: f64, nanosec: bool) -> String {
    if nanosec {
      (1_000_000.0 * tdiff).round().to_string() + "ns"
    } else {
      tdiff.round().to_string() + "ms"
    }
  }

  fn make_assertions(&self, req_body: String, response_body: &String) {
    let first_request_sku = "LGCV7011WS42";
    let second_request_sku = "UXX512FABR569B";
    let contains_first = req_body.contains(first_request_sku) && response_body.contains(first_request_sku);
    let contains_second = req_body.contains(second_request_sku) && response_body.contains(second_request_sku);
    let contains_same_sku_in_some_request = contains_first || contains_second;

    if !contains_same_sku_in_some_request {
      panic!("\t\tresponses differ: {} {}", req_body, response_body);
    }
  }

  async fn send_request(&self, context: &mut Context, pool: &mut Pool, config: &Config) -> (Option<Response>, f64, String) {
    let mut uninterpolator = None;

    // Resolve the name
    let interpolated_name = if self.name.contains('{') {
      uninterpolator.get_or_insert(interpolator::Interpolator::new(context)).resolve(&self.name, !config.relaxed_interpolations)
    } else {
      self.name.clone()
    };

    // Resolve the url
    let interpolated_url = if self.url.contains('{') {
      uninterpolator.get_or_insert(interpolator::Interpolator::new(context)).resolve(&self.url, !config.relaxed_interpolations)
    } else {
      self.url.clone()
    };

    // Resolve relative urls
    let interpolated_base_url = if &interpolated_url[..1] == "/" {
      match context.get("base") {
        Some(value) => {
          if let Some(vs) = value.as_str() {
            format!("{}{}", vs.to_string(), interpolated_url)
          } else {
            panic!("{} Wrong type 'base' variable!", "WARNING!".yellow().bold());
          }
        }
        _ => {
          panic!("{} Unknown 'base' variable!", "WARNING!".yellow().bold());
        }
      }
    } else {
      interpolated_url
    };

    let interpolated_body;
    let url = Url::parse(&interpolated_base_url).expect("Invalid url!");
    let domain = format!("{}://{}:{}", url.scheme(), url.host_str().unwrap(), url.port().unwrap_or(0)); // Unique domain key for keep-alive
    let client = pool.entry(domain).or_insert_with(|| ClientBuilder::default().danger_accept_invalid_certs(config.no_check_certificate).build().unwrap());

    // Method
    let method = match self.method.to_uppercase().as_ref() {
      "GET" => Method::GET,
      "POST" => Method::POST,
      "PUT" => Method::PUT,
      "PATCH" => Method::PATCH,
      "DELETE" => Method::DELETE,
      "HEAD" => Method::HEAD,
      _ => panic!("Unknown method '{}'", self.method),
    };

    // Resolve the body
    let (req_body, request) = if let Some(body) = self.body.as_ref() {
      interpolated_body = uninterpolator.get_or_insert(interpolator::Interpolator::new(context)).resolve(body, !config.relaxed_interpolations);
      (interpolated_body.to_owned(), client.request(method, interpolated_base_url.as_str()).body(interpolated_body))
    } else {
      (interpolated_base_url.as_str().to_owned(), client.request(method, interpolated_base_url.as_str()))
    };

    // Headers
    let mut headers = HeaderMap::new();
    headers.insert(header::USER_AGENT, HeaderValue::from_str(USER_AGENT).unwrap());

    if let Some(cookies) = context.get("cookies") {
      let cookies: Map<String, Value> = serde_json::from_value(cookies.clone()).unwrap();
      let cookie = cookies.iter().map(|(key, value)| format!("{}={}", key, value)).collect::<Vec<_>>().join(";");

      headers.insert(header::COOKIE, HeaderValue::from_str(&cookie).unwrap());
    }

    // Resolve headers
    for (key, val) in self.headers.iter() {
      let interpolated_header = uninterpolator.get_or_insert(interpolator::Interpolator::new(context)).resolve(val, !config.relaxed_interpolations);
      headers.insert(HeaderName::from_bytes(key.as_bytes()).unwrap(), HeaderValue::from_str(&interpolated_header).unwrap());
    }

    let begin = Instant::now();
    // let crequest = request.cloned();
    // let cheaders = headers.clone();
    let response_result = request.headers(headers).timeout(Duration::from_secs(60)).send().await;
    let duration_ms = begin.elapsed().as_secs_f64() * 1000.0;

    match response_result {
      Err(e) => {
        if !config.quiet {
          println!("Error connecting '{}': {:?}", interpolated_base_url.as_str(), e);
        }
        (None, duration_ms, req_body)
      }
      Ok(response) => {
        if !config.quiet {
          let status = response.status();
          let status_text = if status.is_server_error() {
            status.to_string().red()
          } else if status.is_client_error() {
            status.to_string().purple()
          } else {
            status.to_string().yellow()
          };

          println!("{:width$} {} {} {}", interpolated_name.green(), interpolated_base_url.blue().bold(), status_text, Request::format_time(duration_ms, config.nanosec).cyan(), width = 25);
          // request:[{:?}] headers:[{:?}]
          // if let Some(ref key) = self.assign {
          // let decoder = response.body;
          // decoder
          // assert_both(response, req_body);
          // if key.ends_with("_dbg") {
          //   println!("request_body: {:?}",
          //     //crequest,
          //     //cheaders,
          //     req_body);
          // }
          // }
        }

        (Some(response), duration_ms, req_body)
      }
    }
  }
}

pub fn yaml_to_json(data: Yaml) -> Value {
  if let Some(b) = data.as_bool() {
    json!(b)
  } else if let Some(i) = data.as_i64() {
    json!(i)
  } else if let Some(s) = data.as_str() {
    json!(s)
  } else if let Some(h) = data.as_hash() {
    let mut map = Map::new();

    for (key, value) in h.iter() {
      map.entry(key.as_str().unwrap()).or_insert(yaml_to_json(value.clone()));
    }

    json!(map)
  } else if let Some(v) = data.as_vec() {
    let mut array = Vec::new();

    for value in v.iter() {
      array.push(yaml_to_json(value.clone()));
    }

    json!(array)
  } else {
    panic!("Unknown Yaml node")
  }
}

#[async_trait]
impl Runnable for Request {
  async fn execute(&self, context: &mut Context, reports: &mut Reports, pool: &mut Pool, config: &Config) {
    if self.with_item.is_some() {
      context.insert("item".to_string(), yaml_to_json(self.with_item.clone().unwrap()));
    }

    let (res, duration_ms, req_body) = self.send_request(context, pool, config).await;

    match res {
      None => reports.push(Report {
        name: self.name.to_owned(),
        duration: duration_ms,
        status: 520u16,
      }),
      Some(response) => {
        reports.push(Report {
          name: self.name.to_owned(),
          duration: duration_ms,
          status: response.status().as_u16(),
        });

        if response.cookies().count() > 0 {
          let mut cookies = Map::new();

          for cookie in response.cookies() {
            cookies.insert(cookie.name().to_string(), json!(cookie.value().to_string()));
          }

          context.insert("cookies".to_string(), json!(cookies));
        }

        if let Some(ref key) = self.assign {
          let mut headers = Map::new();

          response.headers().iter().for_each(|(header, value)| {
            headers.insert(header.to_string(), json!(value.to_str().unwrap()));
          });

          if key.ends_with("_dbg") {
            println!("request_body: [{:?}]", req_body);
            println!("response: [{:?}]", response);
          }

          let data = response.text().await.unwrap();

          if key.ends_with("_dbg") {
            println!("\tresponse_body: [{}]", data);
          }
          // self.make_assertions(req_body, &data);

          let body: Value = serde_json::from_str(&data).unwrap_or(serde_json::Value::Null);
          let assigned = AssignedRequest {
            body,
            headers,
          };
          let value = serde_json::to_value(assigned).unwrap();

          context.insert(key.to_owned(), value);
        }
      }
    }
  }
}
