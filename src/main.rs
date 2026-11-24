//! Discord Member Dumper
//!
//! Native Windows GUI that collects every member of a Discord guild into
//! a CSV file. Communicates with Discord through the Gateway WebSocket
//! (Op 14 lazy member list), so it works with user tokens — no bot and
//! no elevated permission required, only `VIEW_CHANNEL` on one text
//! channel of the target guild.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use eframe::egui::{
    self, Color32, FontId, Margin, Pos2, Rect, Rounding, Sense, Shadow, Stroke, Ui, Vec2,
};
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::{json, Value};
use std::{
    collections::HashSet,
    fs::File,
    io::Write,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tokio::sync::mpsc;
use tokio_tungstenite::{connect_async, tungstenite::Message};

const API: &str = "https://discord.com/api/v10";
const UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 \
(KHTML, like Gecko) Chrome/125.0.0.0 Safari/537.36 \
Discord/1.0.9036 Chrome/124.0.6367.243 Electron/30.2.0";

const GOLD: Color32 = Color32::from_rgb(230, 180, 80);
const GOLD_SOFT: Color32 = Color32::from_rgb(255, 210, 120);
const BG: Color32 = Color32::from_rgb(10, 10, 12);
const FIELD: Color32 = Color32::from_rgb(22, 22, 28);
const FIELD_HOV: Color32 = Color32::from_rgb(30, 30, 38);
const TEXT: Color32 = Color32::from_rgb(235, 232, 228);
const TEXT_DIM: Color32 = Color32::from_rgb(165, 160, 155);

#[derive(Debug, Deserialize, Clone)]
struct Guild {
    id: String,
    name: String,
}

#[derive(Debug, Deserialize)]
struct Channel {
    id: String,
    #[serde(rename = "type")]
    ty: u32,
}

#[derive(Debug, Deserialize)]
struct Role {
    id: String,
    name: String,
    #[serde(default)]
    permissions: String,
    #[serde(default)]
    position: i32,
}

/// Discord permission bitfield constants. See
/// https://discord.com/developers/docs/topics/permissions
const PERM_KICK: u64         = 1 << 1;
const PERM_BAN: u64          = 1 << 2;
const PERM_ADMIN: u64        = 1 << 3;
const PERM_MANAGE_CHANS: u64 = 1 << 4;
const PERM_MANAGE_GUILD: u64 = 1 << 5;
const PERM_MANAGE_MSGS: u64  = 1 << 13;
const PERM_MANAGE_NICK: u64  = 1 << 27;
const PERM_MANAGE_ROLES: u64 = 1 << 28;
const PERM_MANAGE_WEBHK: u64 = 1 << 29;
const PERM_MODERATE: u64     = 1 << 40;

const MOD_MASK: u64 = PERM_KICK | PERM_BAN | PERM_MANAGE_CHANS | PERM_MANAGE_GUILD
    | PERM_MANAGE_MSGS | PERM_MANAGE_NICK | PERM_MANAGE_ROLES | PERM_MANAGE_WEBHK
    | PERM_MODERATE;

enum Msg {
    Log(String),
    Progress(usize),
    Guilds(Vec<Guild>),
    Done(String),
    Error(String),
}

#[derive(PartialEq, Eq)]
enum Screen {
    Token,
    Servers,
    Running,
}

#[derive(PartialEq, Eq, Copy, Clone)]
enum Filter {
    All,
    Admins,
    Privileged,
}

async fn fetch_guilds(token: String, tx: mpsc::UnboundedSender<Msg>) {
    let client = match reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(20))
        .gzip(true)
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            let _ = tx.send(Msg::Error(format!("client build: {e}")));
            return;
        }
    };
    let resp = match client
        .get(format!("{API}/users/@me/guilds"))
        .header("Authorization", &token)
        .header("User-Agent", UA)
        .header("Accept", "application/json")
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => {
            let _ = tx.send(Msg::Error(format!("guilds fetch: {e}")));
            return;
        }
    };
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        let _ = tx.send(Msg::Error(format!(
            "guilds http {status}: {}",
            &body.chars().take(200).collect::<String>()
        )));
        return;
    }
    let guilds: Vec<Guild> = match resp.json().await {
        Ok(g) => g,
        Err(e) => {
            let _ = tx.send(Msg::Error(format!("guilds parse: {e}")));
            return;
        }
    };
    let _ = tx.send(Msg::Guilds(guilds));
}

fn esc_csv(s: &str) -> String {
    s.replace('"', "\"\"")
}

struct DumpedMember {
    id: String,
    username: String,
    global_name: String,
    nick: String,
    joined_at: String,
    top_role: String,
    top_role_id: String,
    top_role_pos: i32,
    is_admin: bool,
    is_mod: bool,
    role_names: Vec<String>,
}

fn category_rank(m: &DumpedMember) -> u8 {
    if m.is_admin {
        0
    } else if m.is_mod {
        1
    } else {
        2
    }
}

fn category_label(m: &DumpedMember) -> &'static str {
    match category_rank(m) {
        0 => "admin",
        1 => "mod",
        _ => "user",
    }
}

/// Sort admins first, then mods, then regular users; within each group by
/// descending top-role position (so the server owner precedes other admins),
/// then by case-insensitive username.
fn sort_dump(members: &mut [DumpedMember]) {
    members.sort_by(|a, b| {
        category_rank(a)
            .cmp(&category_rank(b))
            .then_with(|| b.top_role_pos.cmp(&a.top_role_pos))
            .then_with(|| a.username.to_lowercase().cmp(&b.username.to_lowercase()))
    });
}

fn write_csv(path: &str, members: &[DumpedMember]) -> std::io::Result<()> {
    let mut out = File::create(path)?;
    writeln!(
        out,
        "category,top_role,top_role_id,username,global_name,id,is_admin,is_mod,joined_at,nick,role_count,all_roles"
    )?;
    for m in members {
        writeln!(
            out,
            "{},\"{}\",{},\"{}\",\"{}\",{},{},{},\"{}\",\"{}\",{},\"{}\"",
            category_label(m),
            esc_csv(&m.top_role),
            m.top_role_id,
            esc_csv(&m.username),
            esc_csv(&m.global_name),
            m.id,
            m.is_admin,
            m.is_mod,
            esc_csv(&m.joined_at),
            esc_csv(&m.nick),
            m.role_names.len(),
            esc_csv(&m.role_names.join("|"))
        )?;
    }
    Ok(())
}

/// Collect every visible guild member through the Discord gateway.
///
/// The REST endpoint `GET /guilds/:id/members` is restricted to bots with
/// the `GUILD_MEMBERS` privileged intent, so this function instead replays
/// the handshake the official client performs: fetch channel + role
/// metadata, open the WebSocket gateway, identify as the stable web
/// client, and walk the lazy member list (Op 14) in 100-member ranges.
/// Each member is resolved against the role map to flag admin / mod
/// status and written incrementally to `out_path`.
async fn scrape(
    token: String,
    guild: u64,
    out_path: String,
    delay_ms: u64,
    filter: Filter,
    tx: mpsc::UnboundedSender<Msg>,
) {
    let _ = tx.send(Msg::Log(format!("[*] fetching channels for guild {guild}")));

    let client = match reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(30))
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            let _ = tx.send(Msg::Error(format!("client build: {e}")));
            return;
        }
    };

    let channels_resp = match client
        .get(format!("{API}/guilds/{guild}/channels"))
        .header("Authorization", &token)
        .header("User-Agent", UA)
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => {
            let _ = tx.send(Msg::Error(format!("channels fetch: {e}")));
            return;
        }
    };
    if !channels_resp.status().is_success() {
        let status = channels_resp.status();
        let body = channels_resp.text().await.unwrap_or_default();
        let _ = tx.send(Msg::Error(format!(
            "channels http {status}: {}",
            &body.chars().take(200).collect::<String>()
        )));
        return;
    }
    let channels: Vec<Channel> = match channels_resp.json().await {
        Ok(v) => v,
        Err(e) => {
            let _ = tx.send(Msg::Error(format!("channels parse: {e}")));
            return;
        }
    };
    let channel_id = match channels.iter().find(|c| c.ty == 0).map(|c| c.id.clone()) {
        Some(id) => id,
        None => {
            let _ = tx.send(Msg::Error("no visible text channel".into()));
            return;
        }
    };
    let _ = tx.send(Msg::Log(format!("[*] using channel {channel_id}")));

    let _ = tx.send(Msg::Log("[*] fetching roles...".into()));
    let roles_resp = match client
        .get(format!("{API}/guilds/{guild}/roles"))
        .header("Authorization", &token)
        .header("User-Agent", UA)
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => {
            let _ = tx.send(Msg::Error(format!("roles fetch: {e}")));
            return;
        }
    };
    let roles: Vec<Role> = if roles_resp.status().is_success() {
        roles_resp.json().await.unwrap_or_default()
    } else {
        let _ = tx.send(Msg::Log(format!(
            "[!] roles http {} — continuing without role data",
            roles_resp.status()
        )));
        Vec::new()
    };
    let mut role_map: std::collections::HashMap<String, (String, u64, i32)> =
        std::collections::HashMap::new();
    let mut everyone_perms: u64 = 0;
    for r in &roles {
        let perms: u64 = r.permissions.parse().unwrap_or(0);
        role_map.insert(r.id.clone(), (r.name.clone(), perms, r.position));
        if r.id == guild.to_string() {
            everyone_perms = perms;
        }
    }
    let _ = tx.send(Msg::Log(format!("[*] loaded {} roles", roles.len())));

    let _ = tx.send(Msg::Log("[*] connecting to gateway...".into()));
    let (ws, _) = match connect_async("wss://gateway.discord.gg/?v=10&encoding=json").await {
        Ok(x) => x,
        Err(e) => {
            let _ = tx.send(Msg::Error(format!("gateway connect: {e}")));
            return;
        }
    };
    let (mut write, mut read) = ws.split();

    let hello_text = match read.next().await {
        Some(Ok(Message::Text(s))) => s,
        Some(Ok(_)) | Some(Err(_)) | None => {
            let _ = tx.send(Msg::Error("no hello frame".into()));
            return;
        }
    };
    let hello: Value = serde_json::from_str(&hello_text).unwrap_or(Value::Null);
    let hb_ms = hello["d"]["heartbeat_interval"].as_u64().unwrap_or(41250);
    let _ = tx.send(Msg::Log(format!("[*] hello, heartbeat {}ms", hb_ms)));

    // Identify as the stable Discord web client. These `properties` fields
    // are checked server-side; omitting them triggers Op 9 Invalid Session.
    let identify = json!({
        "op": 2,
        "d": {
            "token": token,
            "capabilities": 16381,
            "properties": {
                "os": "Windows",
                "browser": "Chrome",
                "device": "",
                "system_locale": "en-US",
                "browser_user_agent": UA,
                "browser_version": "125.0.0.0",
                "os_version": "10",
                "referrer": "",
                "referring_domain": "",
                "referrer_current": "",
                "referring_domain_current": "",
                "release_channel": "stable",
                "client_build_number": 290000,
                "client_event_source": Value::Null
            },
            "presence": {"status": "online", "since": 0, "activities": [], "afk": false},
            "compress": false,
            "client_state": {"guild_versions": {}}
        }
    });
    if write.send(Message::Text(identify.to_string())).await.is_err() {
        let _ = tx.send(Msg::Error("identify send failed".into()));
        return;
    }

    // Single writer task owns the sink half; heartbeat and request tasks
    // enqueue through `send_tx` to avoid shared mutable access.
    let (send_tx, mut send_rx) = tokio::sync::mpsc::channel::<Message>(32);
    let writer_handle = tokio::spawn(async move {
        while let Some(msg) = send_rx.recv().await {
            if write.send(msg).await.is_err() {
                break;
            }
        }
    });

    let seq: Arc<Mutex<Option<u64>>> = Arc::new(Mutex::new(None));
    let seq_hb = seq.clone();
    let hb_send = send_tx.clone();
    let hb_handle = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis((hb_ms as f64 * 0.5) as u64)).await;
        loop {
            let s = *seq_hb.lock().unwrap();
            let hb = json!({"op": 1, "d": s});
            if hb_send.send(Message::Text(hb.to_string())).await.is_err() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(hb_ms)).await;
        }
    });

    let mut dump: Vec<DumpedMember> = Vec::new();
    let mut collected: HashSet<String> = HashSet::new();
    let mut count_admin: usize = 0;
    let mut count_mod: usize = 0;
    let mut count_user: usize = 0;
    let mut range_start: i64 = 0;
    let range_size: i64 = 100;
    let mut empty_streak = 0;
    let guild_id_str = guild.to_string();

    while let Some(msg_res) = read.next().await {
        let msg = match msg_res {
            Ok(m) => m,
            Err(e) => {
                let _ = tx.send(Msg::Error(format!("ws read: {e}")));
                break;
            }
        };
        let text = match msg {
            Message::Text(s) => s,
            Message::Close(_) => break,
            _ => continue,
        };
        let v: Value = match serde_json::from_str(&text) {
            Ok(v) => v,
            Err(_) => continue,
        };
        if let Some(s) = v["s"].as_u64() {
            *seq.lock().unwrap() = Some(s);
        }
        let op = v["op"].as_u64().unwrap_or(99);
        match op {
            0 => {
                let t_name = v["t"].as_str().unwrap_or("");
                match t_name {
                    "READY" => {
                        let _ = tx.send(Msg::Log(
                            "[*] gateway READY, requesting member list".into(),
                        ));
                        let sub = json!({
                            "op": 14,
                            "d": {
                                "guild_id": &guild_id_str,
                                "typing": true,
                                "threads": false,
                                "activities": true,
                                "members": [],
                                "channels": {
                                    &channel_id: [[range_start, range_start + range_size - 1]]
                                },
                                "thread_member_lists": []
                            }
                        });
                        let _ = send_tx.send(Message::Text(sub.to_string())).await;
                    }
                    "GUILD_MEMBER_LIST_UPDATE" => {
                        if v["d"]["guild_id"].as_str() != Some(&guild_id_str) {
                            continue;
                        }
                        let mut new_in_page = 0usize;
                        if let Some(ops) = v["d"]["ops"].as_array() {
                            for op_obj in ops {
                                let op_kind = op_obj["op"].as_str().unwrap_or("");
                                let items = if op_kind == "SYNC" {
                                    op_obj["items"].as_array().cloned().unwrap_or_default()
                                } else if op_kind == "INSERT" || op_kind == "UPDATE" {
                                    vec![op_obj["item"].clone()]
                                } else {
                                    continue;
                                };
                                for item in items {
                                    if let Some(member) = item.get("member") {
                                        let user = &member["user"];
                                        let id = user["id"].as_str().unwrap_or("").to_string();
                                        if id.is_empty() || collected.contains(&id) {
                                            continue;
                                        }
                                        collected.insert(id.clone());
                                        let username = user["username"].as_str().unwrap_or("");
                                        let global = user["global_name"].as_str().unwrap_or("");
                                        let nick = member["nick"].as_str().unwrap_or("");
                                        let joined = member["joined_at"].as_str().unwrap_or("");

                                        let member_role_ids: Vec<String> = member["roles"]
                                            .as_array()
                                            .map(|a| {
                                                a.iter()
                                                    .filter_map(|v| v.as_str().map(String::from))
                                                    .collect()
                                            })
                                            .unwrap_or_default();

                                        let mut combined_perms: u64 = everyone_perms;
                                        let mut top: Option<(String, String, i32)> = None;
                                        let mut role_names: Vec<String> = Vec::new();
                                        for rid in &member_role_ids {
                                            if let Some((name, perms, pos)) = role_map.get(rid) {
                                                combined_perms |= *perms;
                                                role_names.push(name.clone());
                                                match &top {
                                                    None => top = Some((name.clone(), rid.clone(), *pos)),
                                                    Some((_, _, p)) if *pos > *p => {
                                                        top = Some((name.clone(), rid.clone(), *pos))
                                                    }
                                                    _ => {}
                                                }
                                            }
                                        }
                                        let is_admin = combined_perms & PERM_ADMIN != 0;
                                        let is_mod = !is_admin && (combined_perms & MOD_MASK != 0);
                                        let (top_name, top_id, top_pos) = top
                                            .unwrap_or_else(|| ("@everyone".to_string(), String::new(), -1));

                                        if is_admin { count_admin += 1; }
                                        else if is_mod { count_mod += 1; }
                                        else { count_user += 1; }

                                        let pass_filter = match filter {
                                            Filter::All => true,
                                            Filter::Admins => is_admin,
                                            Filter::Privileged => is_admin || is_mod,
                                        };
                                        if pass_filter {
                                            dump.push(DumpedMember {
                                                id: id.clone(),
                                                username: username.to_string(),
                                                global_name: global.to_string(),
                                                nick: nick.to_string(),
                                                joined_at: joined.to_string(),
                                                top_role: top_name,
                                                top_role_id: top_id,
                                                top_role_pos: top_pos,
                                                is_admin,
                                                is_mod,
                                                role_names,
                                            });
                                            new_in_page += 1;
                                        }
                                    }
                                }
                            }
                        }
                        let total = collected.len();
                        let _ = tx.send(Msg::Progress(total));
                        let _ = tx.send(Msg::Log(format!(
                            "[+] range {}-{}: +{} new | admins {} · mods {} · users {} (total {})",
                            range_start,
                            range_start + range_size - 1,
                            new_in_page,
                            count_admin,
                            count_mod,
                            count_user,
                            total
                        )));

                        if new_in_page == 0 {
                            empty_streak += 1;
                            if empty_streak >= 2 {
                                break;
                            }
                        } else {
                            empty_streak = 0;
                        }

                        range_start += range_size;
                        tokio::time::sleep(Duration::from_millis(delay_ms.max(500))).await;
                        let sub = json!({
                            "op": 14,
                            "d": {
                                "guild_id": &guild_id_str,
                                "channels": {
                                    &channel_id: [[range_start, range_start + range_size - 1]]
                                }
                            }
                        });
                        let _ = send_tx.send(Message::Text(sub.to_string())).await;
                    }
                    _ => {}
                }
            }
            7 => {
                let _ = tx.send(Msg::Error("gateway asked for reconnect".into()));
                break;
            }
            9 => {
                let _ = tx.send(Msg::Error("invalid session (token wrong?)".into()));
                break;
            }
            _ => {}
        }
    }

    hb_handle.abort();
    drop(send_tx);
    let _ = writer_handle.await;

    let _ = tx.send(Msg::Log(format!(
        "[*] sorting {} members by category and role...",
        dump.len()
    )));
    sort_dump(&mut dump);

    if let Err(e) = write_csv(&out_path, &dump) {
        let _ = tx.send(Msg::Error(format!("write csv: {e}")));
        return;
    }

    let _ = tx.send(Msg::Done(format!(
        "{} members saved ({} admins, {} mods, {} users) -> {}",
        dump.len(),
        count_admin,
        count_mod,
        count_user,
        out_path
    )));
}

struct App {
    screen: Screen,
    token: String,
    show_token: bool,
    out_path: String,
    delay_ms: String,
    guilds: Vec<Guild>,
    selected_guild: Option<String>,
    guild_filter: String,
    filter: Filter,
    log: Arc<Mutex<Vec<String>>>,
    total: Arc<Mutex<usize>>,
    rx: Option<mpsc::UnboundedReceiver<Msg>>,
    runtime: tokio::runtime::Runtime,
    start_time: Instant,
    btn_hover_t: f32,
    busy: bool,
    stars: Vec<(Pos2, f32)>,
    error_msg: Option<String>,
}

impl Default for App {
    fn default() -> Self {
        let mut stars = Vec::with_capacity(55);
        let mut seed: u32 = 0x1a4c_0ff3;
        let mut rng = || {
            seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
            (seed >> 8) as f32 / (1 << 24) as f32
        };
        for _ in 0..55 {
            stars.push((Pos2::new(rng(), rng()), rng() * 0.8 + 0.2));
        }
        Self {
            screen: Screen::Token,
            token: String::new(),
            show_token: false,
            out_path: "members.csv".into(),
            delay_ms: "750".into(),
            guilds: Vec::new(),
            selected_guild: None,
            guild_filter: String::new(),
            filter: Filter::All,
            log: Arc::new(Mutex::new(Vec::new())),
            total: Arc::new(Mutex::new(0)),
            rx: None,
            runtime: tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .worker_threads(2)
                .build()
                .expect("tokio runtime"),
            start_time: Instant::now(),
            btn_hover_t: 0.0_f32,
            busy: false,
            stars,
            error_msg: None,
        }
    }
}

fn setup_style(ctx: &egui::Context) {
    let mut style = (*ctx.style()).clone();
    style.visuals = egui::Visuals::dark();
    style.visuals.window_fill = BG;
    style.visuals.panel_fill = BG;
    style.visuals.extreme_bg_color = Color32::from_rgb(14, 14, 18);
    style.visuals.faint_bg_color = Color32::from_rgb(18, 18, 22);

    for w in [
        &mut style.visuals.widgets.noninteractive,
        &mut style.visuals.widgets.inactive,
    ] {
        w.bg_fill = FIELD;
        w.weak_bg_fill = FIELD;
        w.rounding = Rounding::same(12.0_f32);
        w.bg_stroke = Stroke::new(1.0_f32, Color32::from_rgba_premultiplied(255, 220, 160, 35));
        w.fg_stroke.color = TEXT;
    }
    style.visuals.widgets.hovered.bg_fill = FIELD_HOV;
    style.visuals.widgets.hovered.weak_bg_fill = FIELD_HOV;
    style.visuals.widgets.hovered.rounding = Rounding::same(12.0_f32);
    style.visuals.widgets.hovered.bg_stroke = Stroke::new(1.3_f32, GOLD);
    style.visuals.widgets.hovered.fg_stroke.color = Color32::WHITE;
    style.visuals.widgets.active.bg_fill = FIELD_HOV;
    style.visuals.widgets.active.weak_bg_fill = FIELD_HOV;
    style.visuals.widgets.active.rounding = Rounding::same(12.0_f32);
    style.visuals.widgets.active.bg_stroke = Stroke::new(1.6_f32, GOLD_SOFT);
    style.visuals.widgets.active.fg_stroke.color = Color32::WHITE;
    style.visuals.widgets.open.bg_fill = FIELD_HOV;
    style.visuals.widgets.open.rounding = Rounding::same(12.0_f32);

    style.visuals.selection.bg_fill = Color32::from_rgba_premultiplied(230, 180, 80, 70);
    style.visuals.selection.stroke = Stroke::new(1.0_f32, GOLD_SOFT);

    style.visuals.window_shadow = Shadow {
        offset: Vec2::new(0.0, 10.0),
        blur: 40.0,
        spread: 0.0,
        color: Color32::from_black_alpha(140),
    };
    style.spacing.item_spacing = Vec2::new(10.0, 10.0);
    style.spacing.button_padding = Vec2::new(16.0, 10.0);
    style.spacing.interact_size.y = 40.0;
    ctx.set_style(style);
}

fn draw_constellation(painter: &egui::Painter, rect: Rect, t: f32, stars: &[(Pos2, f32)]) {
    painter.rect_filled(rect, 0.0, BG);

    let glow_center = Pos2::new(rect.right() + 80.0, rect.bottom() - 60.0);
    for i in 0..10 {
        let frac = i as f32 / 10.0;
        let r = 520.0 * (1.0 - frac * 0.85);
        let a = ((1.0 - frac).powf(3.0) * 14.0) as u8;
        painter.circle_filled(
            glow_center,
            r,
            Color32::from_rgba_unmultiplied(GOLD.r(), GOLD.g(), GOLD.b(), a),
        );
    }

    let w = rect.width();
    let h = rect.height();
    let mut pts: Vec<Pos2> = Vec::with_capacity(stars.len());
    for (i, (p, phase)) in stars.iter().enumerate() {
        let drift = (t * 0.15 + *phase * std::f32::consts::TAU + i as f32 * 0.37).sin() * 1.5;
        let x = rect.left() + p.x * w + drift;
        let y = rect.top() + p.y * h + (t * 0.12 + *phase * 6.28).cos() * 1.5;
        pts.push(Pos2::new(x, y));
    }

    let max_d = 95.0_f32;
    for i in 0..pts.len() {
        for j in (i + 1)..pts.len() {
            let d = (pts[i] - pts[j]).length();
            if d < max_d {
                let a = ((1.0 - d / max_d) * 42.0) as u8;
                painter.line_segment(
                    [pts[i], pts[j]],
                    Stroke::new(
                        0.6_f32,
                        Color32::from_rgba_unmultiplied(GOLD.r(), GOLD.g(), GOLD.b(), a),
                    ),
                );
            }
        }
    }

    for (i, p) in pts.iter().enumerate() {
        let twinkle = 0.5 + 0.5 * (t * 2.0 + i as f32 * 1.3).sin();
        let r = 1.0 + twinkle * 0.7;
        let a = (110.0 + twinkle * 120.0) as u8;
        painter.circle_filled(
            *p,
            r,
            Color32::from_rgba_unmultiplied(GOLD_SOFT.r(), GOLD_SOFT.g(), GOLD_SOFT.b(), a),
        );
    }
}

fn draw_discord_mascot(painter: &egui::Painter, center: Pos2, scale: f32) {
    let w = 42.0 * scale;
    let h = 32.0 * scale;
    let body = Rect::from_center_size(center, Vec2::new(w, h));
    painter.rect_filled(body, Rounding::same(14.0 * scale), Color32::WHITE);
    let horn_r = 5.0 * scale;
    painter.circle_filled(
        Pos2::new(center.x - w * 0.42, center.y + h * 0.45),
        horn_r,
        Color32::WHITE,
    );
    painter.circle_filled(
        Pos2::new(center.x + w * 0.42, center.y + h * 0.45),
        horn_r,
        Color32::WHITE,
    );
    let eye_r = 4.0 * scale;
    painter.circle_filled(
        Pos2::new(center.x - w * 0.18, center.y - h * 0.05),
        eye_r,
        Color32::from_rgb(20, 20, 24),
    );
    painter.circle_filled(
        Pos2::new(center.x + w * 0.18, center.y - h * 0.05),
        eye_r,
        Color32::from_rgb(20, 20, 24),
    );
    painter.circle_filled(
        Pos2::new(center.x, center.y + h * 0.22),
        2.0 * scale,
        Color32::from_rgb(20, 20, 24),
    );
}

fn draw_key_icon(painter: &egui::Painter, center: Pos2, size: f32, col: Color32) {
    let r = size * 0.35;
    let ring = Pos2::new(center.x - size * 0.2, center.y);
    painter.circle_stroke(ring, r, Stroke::new(2.0_f32, col));
    painter.line_segment(
        [
            Pos2::new(ring.x + r, center.y),
            Pos2::new(center.x + size * 0.45, center.y),
        ],
        Stroke::new(2.0_f32, col),
    );
    painter.line_segment(
        [
            Pos2::new(center.x + size * 0.25, center.y),
            Pos2::new(center.x + size * 0.25, center.y + size * 0.18),
        ],
        Stroke::new(2.0_f32, col),
    );
    painter.line_segment(
        [
            Pos2::new(center.x + size * 0.4, center.y),
            Pos2::new(center.x + size * 0.4, center.y + size * 0.14),
        ],
        Stroke::new(2.0_f32, col),
    );
}

fn draw_folder_icon(painter: &egui::Painter, center: Pos2, size: f32, col: Color32) {
    let w = size * 0.9;
    let h = size * 0.65;
    let r = Rect::from_center_size(center, Vec2::new(w, h));
    let tab = Rect::from_min_max(
        Pos2::new(r.left(), r.top() - h * 0.22),
        Pos2::new(r.left() + w * 0.42, r.top() + 2.0),
    );
    painter.rect_stroke(tab, Rounding::same(3.0_f32), Stroke::new(2.0_f32, col));
    painter.rect_stroke(r, Rounding::same(4.0_f32), Stroke::new(2.0_f32, col));
}

fn draw_play_icon(painter: &egui::Painter, center: Pos2, size: f32, col: Color32) {
    let s = size * 0.5;
    let pts = vec![
        Pos2::new(center.x - s * 0.55, center.y - s),
        Pos2::new(center.x + s, center.y),
        Pos2::new(center.x - s * 0.55, center.y + s),
    ];
    painter.add(egui::Shape::convex_polygon(
        pts,
        col,
        Stroke::NONE,
    ));
}

fn draw_search_icon(painter: &egui::Painter, center: Pos2, size: f32, col: Color32) {
    let r = size * 0.35;
    let lens = Pos2::new(center.x - size * 0.1, center.y - size * 0.1);
    painter.circle_stroke(lens, r, Stroke::new(2.0_f32, col));
    painter.line_segment(
        [
            Pos2::new(lens.x + r * 0.7, lens.y + r * 0.7),
            Pos2::new(center.x + size * 0.4, center.y + size * 0.4),
        ],
        Stroke::new(2.0_f32, col),
    );
}

fn gold_button(
    ui: &mut Ui,
    label: &str,
    hover_t: &mut f32,
    enabled: bool,
    with_play: bool,
) -> egui::Response {
    let desired = Vec2::new(ui.available_width().min(420.0), 56.0);
    let (rect, resp) = ui.allocate_exact_size(desired, Sense::click());
    let painter = ui.painter();

    let target: f32 = if resp.hovered() && enabled { 1.0 } else { 0.0 };
    *hover_t += (target - *hover_t) * 0.18;

    let rounding = Rounding::same(14.0_f32);

    if !enabled {
        painter.rect(
            rect,
            rounding,
            Color32::from_rgb(28, 24, 18),
            Stroke::new(1.5_f32, Color32::from_rgb(90, 70, 35)),
        );
    } else {
        let fill = Color32::from_rgb(
            (GOLD.r() as f32 + (GOLD_SOFT.r() as f32 - GOLD.r() as f32) * *hover_t) as u8,
            (GOLD.g() as f32 + (GOLD_SOFT.g() as f32 - GOLD.g() as f32) * *hover_t) as u8,
            (GOLD.b() as f32 + (GOLD_SOFT.b() as f32 - GOLD.b() as f32) * *hover_t) as u8,
        );
        painter.rect(
            rect,
            rounding,
            fill,
            Stroke::new(1.5_f32, Color32::from_rgb(255, 230, 160)),
        );
        let hl = Rect::from_min_max(
            rect.min + Vec2::new(2.0, 2.0),
            Pos2::new(rect.right() - 2.0, rect.top() + rect.height() * 0.45),
        );
        painter.rect_filled(
            hl,
            Rounding {
                nw: 12.0,
                ne: 12.0,
                sw: 0.0,
                se: 0.0,
            },
            Color32::from_rgba_unmultiplied(255, 255, 255, 40),
        );
        let glow_a = (35.0 + 55.0 * *hover_t) as u8;
        painter.rect_stroke(
            rect.expand(4.0_f32),
            Rounding::same(18.0_f32),
            Stroke::new(
                2.5_f32,
                Color32::from_rgba_unmultiplied(GOLD.r(), GOLD.g(), GOLD.b(), glow_a),
            ),
        );
    }

    let text_col = if enabled {
        Color32::from_rgb(30, 22, 10)
    } else {
        TEXT_DIM
    };
    let font = FontId::proportional(18.0);
    let galley = painter.layout_no_wrap(label.to_string(), font.clone(), text_col);
    let text_w = galley.size().x;
    let icon_gap = if with_play { 28.0 } else { 0.0 };
    let total_w = text_w + icon_gap;
    let text_x = rect.center().x - total_w * 0.5 + icon_gap;
    if with_play {
        draw_play_icon(
            painter,
            Pos2::new(text_x - 14.0 - 6.0, rect.center().y),
            18.0,
            text_col,
        );
    }
    painter.galley(
        Pos2::new(text_x, rect.center().y - galley.size().y * 0.5),
        galley,
        text_col,
    );

    resp
}

fn label_with_icon(ui: &mut Ui, text: &str, kind: &str) {
    let h = 24.0;
    let (rect, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), h), Sense::hover());
    let painter = ui.painter();
    let icon_center = Pos2::new(rect.left() + 12.0, rect.center().y);
    match kind {
        "key" => draw_key_icon(painter, icon_center, 20.0, GOLD),
        "folder" => draw_folder_icon(painter, icon_center, 20.0, GOLD),
        "search" => draw_search_icon(painter, icon_center, 20.0, GOLD),
        _ => {}
    }
    painter.text(
        Pos2::new(rect.left() + 32.0, rect.center().y + 0.5),
        egui::Align2::LEFT_CENTER,
        text,
        FontId::proportional(14.0),
        TEXT,
    );
}

fn field_input(
    ui: &mut Ui,
    value: &mut String,
    hint: &str,
    password: bool,
    enabled: bool,
    width: f32,
) {
    ui.add_enabled(
        enabled,
        egui::TextEdit::singleline(value)
            .password(password)
            .hint_text(hint)
            .margin(Vec2::new(14.0, 11.0))
            .desired_width(width)
            .font(FontId::proportional(14.0)),
    );
}

fn radio_pill(ui: &mut Ui, label: &str, active: bool, enabled: bool) -> egui::Response {
    let h = 36.0;
    let font = FontId::proportional(13.0);
    let galley = ui
        .painter()
        .layout_no_wrap(label.to_string(), font.clone(), TEXT);
    let text_w = galley.size().x;
    let w = text_w + 54.0;
    let (rect, resp) = ui.allocate_exact_size(
        Vec2::new(w, h),
        if enabled { Sense::click() } else { Sense::hover() },
    );
    let p = ui.painter();
    let hovered = resp.hovered() && enabled;
    let fill = if active {
        Color32::from_rgba_unmultiplied(230, 180, 80, 40)
    } else if hovered {
        FIELD_HOV
    } else {
        FIELD
    };
    let border = if active {
        GOLD
    } else if hovered {
        GOLD_SOFT
    } else {
        Color32::from_rgba_unmultiplied(255, 220, 160, 55)
    };
    p.rect(
        rect,
        Rounding::same(999.0_f32),
        fill,
        Stroke::new(1.0_f32, border),
    );
    let dot_c = Pos2::new(rect.left() + 20.0, rect.center().y);
    p.circle_stroke(
        dot_c,
        7.5,
        Stroke::new(
            1.6_f32,
            if active {
                GOLD_SOFT
            } else {
                Color32::from_rgb(150, 150, 160)
            },
        ),
    );
    if active {
        p.circle_filled(dot_c, 4.0, GOLD);
    }
    let text_col = if active { GOLD_SOFT } else { TEXT };
    p.text(
        Pos2::new(rect.left() + 36.0, rect.center().y),
        egui::Align2::LEFT_CENTER,
        label,
        font,
        text_col,
    );
    resp
}

fn icon_button(ui: &mut Ui, kind: &str, enabled: bool) -> egui::Response {
    let size = Vec2::new(42.0, 42.0);
    let (rect, resp) = ui.allocate_exact_size(size, Sense::click());
    let painter = ui.painter();
    let hovered = resp.hovered() && enabled;
    let fill = if hovered { FIELD_HOV } else { FIELD };
    let border = if hovered { GOLD } else {
        Color32::from_rgba_premultiplied(255, 220, 160, 40)
    };
    painter.rect(
        rect,
        Rounding::same(12.0_f32),
        fill,
        Stroke::new(1.0_f32, border),
    );
    let c = rect.center();
    let col = if enabled { GOLD } else { TEXT_DIM };
    match kind {
        "folder" => draw_folder_icon(painter, c, 22.0, col),
        "search" => draw_search_icon(painter, c, 20.0, col),
        _ => {}
    }
    resp
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        ctx.request_repaint_after(Duration::from_millis(16));

        if let Some(rx) = self.rx.as_mut() {
            while let Ok(msg) = rx.try_recv() {
                match msg {
                    Msg::Log(s) => self.log.lock().unwrap().push(s),
                    Msg::Progress(n) => *self.total.lock().unwrap() = n,
                    Msg::Guilds(mut g) => {
                        g.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
                        self.guilds = g;
                        self.busy = false;
                        self.screen = Screen::Servers;
                    }
                    Msg::Done(s) => {
                        self.log.lock().unwrap().push(format!("[done] {s}"));
                        self.busy = false;
                    }
                    Msg::Error(e) => {
                        self.error_msg = Some(e.clone());
                        self.log.lock().unwrap().push(format!("[error] {e}"));
                        self.busy = false;
                    }
                }
            }
        }

        let t = self.start_time.elapsed().as_secs_f32();

        egui::CentralPanel::default()
            .frame(egui::Frame::none().fill(BG))
            .show(ctx, |ui| {
                let full = ui.max_rect();
                let stars = self.stars.clone();
                draw_constellation(ui.painter(), full, t, &stars);

                let avail = ui.available_width();
                let col_w = 620.0_f32.min(avail - 48.0);
                let pad = ((avail - col_w) * 0.5).max(24.0);
                egui::Frame::none()
                    .inner_margin(Margin {
                        left: pad,
                        right: pad,
                        top: 28.0,
                        bottom: 28.0,
                    })
                    .show(ui, |ui| {
                        ui.set_max_width(col_w);
                        match self.screen {
                            Screen::Token => self.draw_token_screen(ui, t),
                            Screen::Servers => self.draw_servers_screen(ui, t),
                            Screen::Running => self.draw_running_screen(ui, t),
                        }
                    });
            });
    }
}

impl App {
    fn draw_token_screen(&mut self, ui: &mut Ui, _t: f32) {
        ui.horizontal(|ui| {
            ui.add_space(0.0);
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let text = "built by Axthony";
                let font = FontId::proportional(12.5);
                let galley = ui.painter().layout_no_wrap(text.to_string(), font.clone(), TEXT_DIM);
                let pad = Vec2::new(12.0, 7.0);
                let size = galley.size() + pad * 2.0;
                let (rect, _) = ui.allocate_exact_size(size, Sense::hover());
                let p = ui.painter();
                p.rect(
                    rect,
                    Rounding::same(999.0_f32),
                    FIELD,
                    Stroke::new(1.0_f32, Color32::from_rgba_premultiplied(255, 220, 160, 45)),
                );
                let dim_g = p.layout_no_wrap("built by ".to_string(), font.clone(), TEXT_DIM);
                let gold_g = p.layout_no_wrap("Axthony".to_string(), font.clone(), GOLD_SOFT);
                let total_w = dim_g.size().x + gold_g.size().x;
                let start_x = rect.center().x - total_w * 0.5;
                p.galley(
                    Pos2::new(start_x, rect.center().y - dim_g.size().y * 0.5),
                    dim_g.clone(),
                    TEXT_DIM,
                );
                p.galley(
                    Pos2::new(
                        start_x + dim_g.size().x,
                        rect.center().y - gold_g.size().y * 0.5,
                    ),
                    gold_g,
                    GOLD_SOFT,
                );
            });
        });

        ui.add_space(30.0);

        ui.horizontal(|ui| {
            let (rect, _) =
                ui.allocate_exact_size(Vec2::new(70.0, 60.0), Sense::hover());
            draw_discord_mascot(ui.painter(), rect.center(), 1.1);
            ui.painter().line_segment(
                [
                    Pos2::new(rect.right() + 4.0, rect.top() + 8.0),
                    Pos2::new(rect.right() + 4.0, rect.bottom() - 8.0),
                ],
                Stroke::new(2.0_f32, GOLD),
            );
            ui.add_space(14.0);
            ui.vertical(|ui| {
                ui.horizontal(|ui| {
                    ui.label(
                        egui::RichText::new("Discord Member ")
                            .color(Color32::WHITE)
                            .size(34.0)
                            .strong(),
                    );
                    ui.label(
                        egui::RichText::new("Dumper")
                            .color(GOLD_SOFT)
                            .size(34.0)
                            .strong(),
                    );
                });
            });
        });

        ui.add_space(4.0);
        ui.label(
            egui::RichText::new("Paste your user token, drop the guild ID, hit start.")
                .color(TEXT_DIM)
                .size(14.0),
        );

        ui.add_space(26.0);

        label_with_icon(ui, "Token", "key");
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            let btn_w = 80.0;
            field_input(
                ui,
                &mut self.token,
                "user token (no 'Bot ' prefix)",
                !self.show_token,
                !self.busy,
                ui.available_width() - btn_w - 10.0,
            );
            let label = if self.show_token { "hide" } else { "show" };
            if ui
                .add_enabled(
                    !self.busy,
                    egui::Button::new(
                        egui::RichText::new(label).color(GOLD_SOFT).size(13.5),
                    )
                    .min_size(Vec2::new(btn_w, 42.0))
                    .rounding(Rounding::same(12.0_f32))
                    .fill(FIELD)
                    .stroke(Stroke::new(1.0_f32, GOLD)),
                )
                .clicked()
            {
                self.show_token = !self.show_token;
            }
        });

        ui.add_space(14.0);

        label_with_icon(ui, "Output file", "folder");
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            let btn_w = 52.0;
            field_input(
                ui,
                &mut self.out_path,
                "members.csv",
                false,
                !self.busy,
                ui.available_width() - btn_w - 10.0,
            );
            if icon_button(ui, "folder", !self.busy).clicked() {
                if let Some(path) = rfd::FileDialog::new()
                    .set_title("Save members to...")
                    .add_filter("CSV", &["csv"])
                    .set_file_name(&self.out_path)
                    .save_file()
                {
                    if let Some(s) = path.to_str() {
                        self.out_path = s.to_string();
                    }
                }
            }
        });

        ui.add_space(30.0);

        ui.vertical_centered(|ui| {
            let enabled = !self.busy && !self.token.trim().is_empty();
            let label = if self.busy { "Loading..." } else { "Start" };
            if gold_button(ui, label, &mut self.btn_hover_t, enabled, true).clicked() && enabled {
                self.busy = true;
                self.error_msg = None;
                let (tx, rx) = mpsc::unbounded_channel();
                self.rx = Some(rx);
                let token = self.token.trim().to_string();
                self.runtime.spawn(fetch_guilds(token, tx));
            }
        });

        if let Some(err) = &self.error_msg {
            ui.add_space(14.0);
            ui.vertical_centered(|ui| {
                ui.label(
                    egui::RichText::new(err)
                        .color(Color32::from_rgb(255, 130, 140))
                        .size(12.5),
                );
            });
        }
    }

    fn draw_servers_screen(&mut self, ui: &mut Ui, _t: f32) {
        ui.horizontal(|ui| {
            if ui
                .add(
                    egui::Button::new(
                        egui::RichText::new("<  back").color(GOLD_SOFT).size(13.0),
                    )
                    .min_size(Vec2::new(90.0, 36.0))
                    .rounding(Rounding::same(10.0_f32))
                    .fill(FIELD)
                    .stroke(Stroke::new(1.0_f32, GOLD)),
                )
                .clicked()
            {
                self.screen = Screen::Token;
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.label(
                    egui::RichText::new(format!("{} servers", self.guilds.len()))
                        .color(TEXT_DIM)
                        .size(13.0),
                );
            });
        });

        ui.add_space(14.0);

        ui.horizontal(|ui| {
            let (rect, _) =
                ui.allocate_exact_size(Vec2::new(60.0, 50.0), Sense::hover());
            draw_discord_mascot(ui.painter(), rect.center(), 0.9);
            ui.painter().line_segment(
                [
                    Pos2::new(rect.right() + 4.0, rect.top() + 8.0),
                    Pos2::new(rect.right() + 4.0, rect.bottom() - 8.0),
                ],
                Stroke::new(2.0_f32, GOLD),
            );
            ui.add_space(12.0);
            ui.horizontal(|ui| {
                ui.label(
                    egui::RichText::new("Choose a ")
                        .color(Color32::WHITE)
                        .size(26.0)
                        .strong(),
                );
                ui.label(
                    egui::RichText::new("Server")
                        .color(GOLD_SOFT)
                        .size(26.0)
                        .strong(),
                );
            });
        });

        ui.add_space(14.0);

        label_with_icon(ui, "Search", "search");
        ui.add_space(4.0);
        field_input(
            ui,
            &mut self.guild_filter,
            "filter by name...",
            false,
            true,
            f32::INFINITY,
        );

        ui.add_space(12.0);

        let filter = self.guild_filter.to_lowercase();
        let mut clicked_id: Option<String> = None;

        egui::Frame::none()
            .fill(Color32::from_rgba_premultiplied(0, 0, 0, 140))
            .stroke(Stroke::new(
                1.0_f32,
                Color32::from_rgba_premultiplied(255, 220, 160, 35),
            ))
            .rounding(Rounding::same(14.0_f32))
            .inner_margin(Margin::same(10.0))
            .show(ui, |ui| {
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .max_height(260.0)
                    .show(ui, |ui| {
                        let items: Vec<&Guild> = self
                            .guilds
                            .iter()
                            .filter(|g| filter.is_empty() || g.name.to_lowercase().contains(&filter))
                            .collect();
                        for g in items {
                            let selected = self.selected_guild.as_deref() == Some(&g.id);
                            let row_h = 56.0;
                            let (rect, resp) = ui.allocate_exact_size(
                                Vec2::new(ui.available_width(), row_h),
                                Sense::click(),
                            );
                            let painter = ui.painter();
                            let hover = resp.hovered();
                            let fill = if selected {
                                Color32::from_rgba_premultiplied(230, 180, 80, 40)
                            } else if hover {
                                FIELD_HOV
                            } else {
                                FIELD
                            };
                            let border = if selected || hover {
                                GOLD
                            } else {
                                Color32::from_rgba_premultiplied(255, 220, 160, 30)
                            };
                            painter.rect(
                                rect.shrink(2.0_f32),
                                Rounding::same(10.0_f32),
                                fill,
                                Stroke::new(1.0_f32, border),
                            );
                            let av = Pos2::new(rect.left() + 32.0, rect.center().y);
                            painter.circle_filled(
                                av,
                                18.0,
                                Color32::from_rgb(40, 32, 20),
                            );
                            painter.circle_stroke(
                                av,
                                18.0,
                                Stroke::new(1.5_f32, GOLD),
                            );
                            let letter = g
                                .name
                                .chars()
                                .next()
                                .map(|c| c.to_ascii_uppercase().to_string())
                                .unwrap_or_else(|| "#".into());
                            painter.text(
                                av,
                                egui::Align2::CENTER_CENTER,
                                letter,
                                FontId::proportional(16.0),
                                GOLD_SOFT,
                            );
                            painter.text(
                                Pos2::new(rect.left() + 64.0, rect.center().y - 8.0),
                                egui::Align2::LEFT_CENTER,
                                &g.name,
                                FontId::proportional(14.5),
                                TEXT,
                            );
                            painter.text(
                                Pos2::new(rect.left() + 64.0, rect.center().y + 10.0),
                                egui::Align2::LEFT_CENTER,
                                &g.id,
                                FontId::proportional(11.0),
                                TEXT_DIM,
                            );
                            if selected {
                                let cx = rect.right() - 28.0;
                                let cy = rect.center().y;
                                painter.circle_filled(Pos2::new(cx, cy), 10.0, GOLD);
                                painter.line_segment(
                                    [
                                        Pos2::new(cx - 4.0, cy),
                                        Pos2::new(cx - 1.0, cy + 3.0),
                                    ],
                                    Stroke::new(2.0_f32, BG),
                                );
                                painter.line_segment(
                                    [
                                        Pos2::new(cx - 1.0, cy + 3.0),
                                        Pos2::new(cx + 5.0, cy - 4.0),
                                    ],
                                    Stroke::new(2.0_f32, BG),
                                );
                            }
                            if resp.clicked() {
                                clicked_id = Some(g.id.clone());
                            }
                        }
                    });
            });

        if let Some(id) = clicked_id {
            self.selected_guild = Some(id);
        }

        ui.add_space(16.0);

        label_with_icon(ui, "Output file", "folder");
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            let btn_w = 52.0;
            field_input(
                ui,
                &mut self.out_path,
                "members.csv",
                false,
                true,
                ui.available_width() - btn_w - 10.0,
            );
            if icon_button(ui, "folder", true).clicked() {
                if let Some(path) = rfd::FileDialog::new()
                    .set_title("Save members to...")
                    .add_filter("CSV", &["csv"])
                    .set_file_name(&self.out_path)
                    .save_file()
                {
                    if let Some(s) = path.to_str() {
                        self.out_path = s.to_string();
                    }
                }
            }
        });

        ui.add_space(14.0);

        label_with_icon(ui, "Filter (optional)", "search");
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            if radio_pill(ui, "All members", self.filter == Filter::All, !self.busy).clicked() {
                self.filter = Filter::All;
            }
            ui.add_space(8.0);
            if radio_pill(ui, "Only admins", self.filter == Filter::Admins, !self.busy).clicked() {
                self.filter = Filter::Admins;
            }
            ui.add_space(8.0);
            if radio_pill(
                ui,
                "Admins + mods",
                self.filter == Filter::Privileged,
                !self.busy,
            )
            .clicked()
            {
                self.filter = Filter::Privileged;
            }
        });

        ui.add_space(14.0);

        ui.vertical_centered(|ui| {
            let enabled = self.selected_guild.is_some() && !self.out_path.trim().is_empty();
            if gold_button(ui, "Start dump", &mut self.btn_hover_t, enabled, true).clicked()
                && enabled
            {
                let guild_id: u64 = self.selected_guild.as_ref().unwrap().parse().unwrap_or(0);
                if guild_id == 0 {
                    self.error_msg = Some("invalid guild id".into());
                    return;
                }
                let (tx, rx) = mpsc::unbounded_channel();
                self.rx = Some(rx);
                self.busy = true;
                self.log.lock().unwrap().clear();
                *self.total.lock().unwrap() = 0;
                self.screen = Screen::Running;
                let delay_ms: u64 = self.delay_ms.trim().parse().unwrap_or(750);
                let token = self.token.trim().to_string();
                let out = self.out_path.trim().to_string();
                let filter = self.filter;
                self.runtime
                    .spawn(scrape(token, guild_id, out, delay_ms, filter, tx));
            }
        });
    }

    fn draw_running_screen(&mut self, ui: &mut Ui, t: f32) {
        ui.horizontal(|ui| {
            if ui
                .add(
                    egui::Button::new(
                        egui::RichText::new("<  servers").color(GOLD_SOFT).size(13.0),
                    )
                    .min_size(Vec2::new(110.0, 36.0))
                    .rounding(Rounding::same(10.0_f32))
                    .fill(FIELD)
                    .stroke(Stroke::new(1.0_f32, GOLD)),
                )
                .clicked()
                && !self.busy
            {
                self.screen = Screen::Servers;
            }
        });

        ui.add_space(14.0);

        ui.horizontal(|ui| {
            let (rect, _) =
                ui.allocate_exact_size(Vec2::new(60.0, 50.0), Sense::hover());
            draw_discord_mascot(ui.painter(), rect.center(), 0.9);
            ui.painter().line_segment(
                [
                    Pos2::new(rect.right() + 4.0, rect.top() + 8.0),
                    Pos2::new(rect.right() + 4.0, rect.bottom() - 8.0),
                ],
                Stroke::new(2.0_f32, GOLD),
            );
            ui.add_space(12.0);
            ui.horizontal(|ui| {
                ui.label(
                    egui::RichText::new("Dumping ")
                        .color(Color32::WHITE)
                        .size(26.0)
                        .strong(),
                );
                ui.label(
                    egui::RichText::new("Members")
                        .color(GOLD_SOFT)
                        .size(26.0)
                        .strong(),
                );
            });
        });

        ui.add_space(14.0);

        ui.horizontal(|ui| {
            if self.busy {
                let size = 22.0_f32;
                let (rect, _) = ui.allocate_exact_size(Vec2::splat(size), Sense::hover());
                let painter = ui.painter();
                let center = rect.center();
                let r = size * 0.45;
                for i in 0..8 {
                    let ang = t * 4.0 + i as f32 * std::f32::consts::TAU / 8.0;
                    let a = (i as f32 / 8.0 * 255.0) as u8;
                    let p = Pos2::new(center.x + ang.cos() * r, center.y + ang.sin() * r);
                    painter.circle_filled(
                        p,
                        2.6,
                        Color32::from_rgba_premultiplied(GOLD.r(), GOLD.g(), GOLD.b(), a),
                    );
                }
                ui.add_space(6.0);
            }
            let total = *self.total.lock().unwrap();
            ui.label(
                egui::RichText::new(format!("Members pulled: {total}"))
                    .color(TEXT)
                    .size(14.0),
            );
        });

        ui.add_space(10.0);

        egui::Frame::none()
            .fill(Color32::from_rgba_premultiplied(0, 0, 0, 140))
            .stroke(Stroke::new(
                1.0_f32,
                Color32::from_rgba_premultiplied(255, 220, 160, 35),
            ))
            .rounding(Rounding::same(14.0_f32))
            .inner_margin(Margin::same(14.0))
            .show(ui, |ui| {
                egui::ScrollArea::vertical()
                    .stick_to_bottom(true)
                    .auto_shrink([false, false])
                    .min_scrolled_height(300.0)
                    .max_height(380.0)
                    .show(ui, |ui| {
                        let log = self.log.lock().unwrap();
                        if log.is_empty() {
                            ui.label(
                                egui::RichText::new("(waiting...)")
                                    .color(TEXT_DIM)
                                    .monospace()
                                    .size(12.0),
                            );
                        }
                        for line in log.iter() {
                            let col = if line.starts_with("[error]") {
                                Color32::from_rgb(255, 120, 130)
                            } else if line.starts_with("[done]") {
                                Color32::from_rgb(140, 230, 160)
                            } else if line.starts_with("[+]") {
                                GOLD_SOFT
                            } else {
                                TEXT
                            };
                            ui.label(
                                egui::RichText::new(line)
                                    .color(col)
                                    .monospace()
                                    .size(12.5),
                            );
                        }
                    });
            });
    }
}

fn main() -> eframe::Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([860.0, 920.0])
            .with_min_inner_size([760.0, 820.0])
            .with_title("Discord Member Dumper")
            .with_decorations(true)
            .with_transparent(false),
        ..Default::default()
    };
    eframe::run_native(
        "Discord Member Dumper",
        options,
        Box::new(|cc| {
            setup_style(&cc.egui_ctx);
            Ok(Box::new(App::default()))
        }),
    )
}
