//! `esc`: the ESC relay, a Star of the Ring.

use std::fs::{self, File};
use std::io::Read;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use esc::bridge::Bridge;
use esc::ids::{id64, new_id64};
use esc::json::{self, Js};
use esc::lobby::Lobby;
use esc::{courier, gate, http};

const USAGE: &str = "\
esc - Extremely Simple Communication: a relay, a Star of the Ring

usage:
  esc serve [--data DIR] [--listen ADDR] [--web DIR]
      Run this Star. The first run creates DIR and prints a one-time admin Ticket.
  esc join <url-of-any-star> <spark> [--data DIR] [--url MY_PUBLIC_URL] [--name NAME]
      Light a new Star: join an existing Ring with a Spark from the Control Tower.
  esc version

DIR defaults to $ESC_DATA, then ./esc_data. ADDR defaults to 127.0.0.1:7878.";

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let flag = |name: &str| args.windows(2).find(|w| w[0] == name).map(|w| w[1].clone());
    let data = PathBuf::from(flag("--data").or_else(|| std::env::var("ESC_DATA").ok()).unwrap_or_else(|| "esc_data".into()));
    let code = match args.first().map(String::as_str).unwrap_or("serve") {
        "serve" => serve(&data, flag("--listen"), flag("--web").map(PathBuf::from)),
        "join" => join(&data, &args, flag("--url"), flag("--name")),
        "version" | "--version" | "-V" => {
            println!("esc {}", env!("CARGO_PKG_VERSION"));
            0
        }
        "help" | "--help" | "-h" => {
            println!("{USAGE}");
            0
        }
        other => {
            eprintln!("esc: unknown command {other:?}\n\n{USAGE}");
            2
        }
    };
    std::process::exit(code);
}

fn serve(data: &Path, listen: Option<String>, web: Option<PathBuf>) -> i32 {
    if let Err(e) = fs::create_dir_all(data) {
        eprintln!("esc: cannot create {}: {e}", data.display());
        return 1;
    }
    // One Star per data folder.
    let lock = match File::create(data.join("lock")) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("esc: cannot lock {}: {e}", data.display());
            return 1;
        }
    };
    if lock.try_lock().is_err() {
        eprintln!("esc: another Star is already running on {}", data.display());
        return 1;
    }
    let opened = match Bridge::open(data, listen.as_deref()) {
        Ok(o) => o,
        Err(e) => {
            eprintln!("esc: cannot open {}: {e}", data.display());
            return 1;
        }
    };
    let bridge = opened.bridge.clone();
    let listener = match TcpListener::bind(&bridge.listen) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("esc: cannot listen on {}: {e}", bridge.listen);
            return 1;
        }
    };
    let lobby = Arc::new(Lobby::new(web.clone()));
    let local = format!("http://{}", bridge.listen);
    println!("ESC · Star \"{}\" · v{}", bridge.name, env!("CARGO_PKG_VERSION"));
    println!("  data    {}  ({} events replayed)", data.display(), opened.replayed);
    println!("  listen  {local}");
    match &web {
        Some(d) => println!("  app     served from {} (dev mode)", d.display()),
        None => println!("  app     {:.1} KB embedded", lobby.total_bytes() as f64 / 1024.0),
    }
    if let Some(t) = &opened.admin_invite {
        println!("\n  No admin yet. Open this once to create the first (admin) account:\n  {local}/#ticket={t}\n");
    }
    // Dustin, the recycling robot, makes his round every minute.
    let d = bridge.clone();
    let _ = thread::Builder::new().name("dustin".into()).spawn(move || {
        loop {
            thread::sleep(Duration::from_secs(60));
            d.dustin();
        }
    });
    esc::ring::start(bridge.clone());
    http::serve(listener, 4096, gate::handler(bridge, lobby));
    0
}

fn join(data: &Path, args: &[String], url: Option<String>, name: Option<String>) -> i32 {
    let (Some(peer), Some(spark)) = (args.get(1), args.get(2)) else {
        eprintln!("{USAGE}");
        return 2;
    };
    if data.join("relay.ini").exists() {
        eprintln!("esc: {} already holds a Star", data.display());
        return 1;
    }
    let peer = peer.trim_end_matches('/');
    let id = new_id64();
    let mut body = Js::new();
    body.obj().ks("t", spark).ks("id", &id64(id)).ks("u", url.as_deref().unwrap_or(""));
    if let Some(n) = &name {
        body.ks("n", n);
    }
    body.end_obj();
    let resp = courier::request(
        "POST",
        &format!("{peer}/ring/join"),
        &[("Content-Type", "application/json")],
        Some(body.finish().into_bytes()),
        Duration::from_secs(30),
    );
    let mut resp = match resp {
        Ok(r) => r,
        Err(e) => {
            eprintln!("esc: cannot reach {peer}: {e}");
            return 1;
        }
    };
    let mut text = String::new();
    let _ = resp.body.read_to_string(&mut text);
    let j = match json::parse(&text) {
        Ok(j) if resp.status == 200 => j,
        Ok(j) => {
            eprintln!("esc: join refused ({}): {}", resp.status, j.s("e").unwrap_or(&text));
            return 1;
        }
        Err(_) => {
            eprintln!("esc: join failed ({}): {text}", resp.status);
            return 1;
        }
    };
    let (Some(key), Some(pepper), Some(star)) = (j.s("key"), j.s("pepper"), j.s("name")) else {
        eprintln!("esc: join answer is incomplete");
        return 1;
    };
    let mut seeds: Vec<String> = vec![peer.to_string()];
    for p in j.a("peers") {
        if let Some(u) = p.s("u").filter(|u| !u.is_empty()) {
            seeds.push(u.to_string());
        }
    }
    seeds.dedup();
    let written = fs::create_dir_all(data.join("log"))
        .and_then(|_| fs::write(data.join("secret"), format!("pepper = {pepper}\nring = {key}\n")))
        .and_then(|_| {
            fs::write(
                data.join("relay.ini"),
                format!(
                    "# ESC relay (a Star of the Ring)\nid = {}\nname = {star}\nlisten = 127.0.0.1:7878\nurl = {}\nseeds = {}\n",
                    id64(id),
                    url.as_deref().unwrap_or(""),
                    seeds.join(",")
                ),
            )
        });
    if let Err(e) = written {
        eprintln!("esc: cannot write {}: {e}", data.display());
        return 1;
    }
    println!("Star \"{star}\" is lit and part of the Ring.\nNow run:  esc serve --data {}", data.display());
    0
}
