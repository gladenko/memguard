// Copyright (C) 2026 Gianluca Rettore
// SPDX-License-Identifier: GPL-3.0-or-later

//! memguard: monitora la memoria, avvisa quando è satura e la libera
//! quando il sistema sta per rallentare. Funziona su KDE Plasma e GNOME
//! (notifiche via org.freedesktop.Notifications) e non richiede root.

use std::collections::HashMap;
use std::env;
use std::fs;
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::sleep;
use std::time::{Duration, Instant};

use ksni::blocking::TrayMethods;

// ---------------------------------------------------------------- config

struct Config {
    interval: Duration,
    warn_pct: f64,     // % RAM usata oltre cui avvisare
    critical_pct: f64, // % RAM usata oltre cui liberare
    psi_warn: f64,     // PSI "some" avg10 oltre cui avvisare
    psi_critical: f64, // PSI "some" avg10 oltre cui liberare
    swap_critical: f64,
    cooldown: Duration, // pausa minima tra due notifiche / due pulizie
    dry_run: bool,
    once: bool,
    root_clean: bool,
    no_tray: bool,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            interval: Duration::from_secs(5),
            warn_pct: 85.0,
            critical_pct: 93.0,
            psi_warn: 10.0,
            psi_critical: 25.0,
            swap_critical: 60.0,
            cooldown: Duration::from_secs(120),
            dry_run: false,
            once: false,
            root_clean: false,
            no_tray: false,
        }
    }
}

const HELP: &str = "memguard - monitor memoria per KDE/GNOME

USO: memguard [opzioni]
  --interval <s>      secondi tra i controlli (default 5)
  --warn <pct>        soglia avviso RAM usata (default 85)
  --critical <pct>    soglia pulizia RAM usata (default 93)
  --psi-warn <n>      soglia avviso pressione PSI avg10 (default 10)
  --psi-critical <n>  soglia pulizia pressione PSI avg10 (default 25)
  --swap <pct>        soglia pulizia swap usata (default 60)
  --cooldown <s>      pausa tra notifiche/pulizie (default 120)
  --dry-run           non libera nulla, notifica soltanto
  --root-clean        svuota anche la cache di sistema (richiede password polkit)
  --no-tray           non mostra l'icona nella barra
  --once              stampa lo stato ed esce
  -h, --help          mostra questo aiuto";

fn parse_args() -> Config {
    let mut c = Config::default();
    let mut args = env::args().skip(1);
    while let Some(a) = args.next() {
        let mut val = |name: &str| -> f64 {
            args.next()
                .and_then(|v| v.parse().ok())
                .unwrap_or_else(|| {
                    eprintln!("valore mancante o non valido per {name}");
                    std::process::exit(2)
                })
        };
        match a.as_str() {
            "--interval" => c.interval = Duration::from_secs_f64(val("--interval").max(1.0)),
            "--warn" => c.warn_pct = val("--warn"),
            "--critical" => c.critical_pct = val("--critical"),
            "--psi-warn" => c.psi_warn = val("--psi-warn"),
            "--psi-critical" => c.psi_critical = val("--psi-critical"),
            "--swap" => c.swap_critical = val("--swap"),
            "--cooldown" => c.cooldown = Duration::from_secs_f64(val("--cooldown")),
            "--dry-run" => c.dry_run = true,
            "--once" => c.once = true,
            "--root-clean" => c.root_clean = true,
            "--no-tray" => c.no_tray = true,
            "-h" | "--help" => {
                println!("{HELP}");
                std::process::exit(0)
            }
            other => {
                eprintln!("opzione sconosciuta: {other}\n\n{HELP}");
                std::process::exit(2)
            }
        }
    }
    if c.warn_pct >= c.critical_pct {
        eprintln!("--warn deve essere minore di --critical");
        std::process::exit(2);
    }
    c
}

// ---------------------------------------------------------------- lettura stato

#[derive(Debug, Clone, Copy)]
struct MemInfo {
    total_kb: u64,
    available_kb: u64,
    swap_total_kb: u64,
    swap_free_kb: u64,
}

impl MemInfo {
    fn used_pct(&self) -> f64 {
        if self.total_kb == 0 {
            return 0.0;
        }
        100.0 * (self.total_kb.saturating_sub(self.available_kb)) as f64 / self.total_kb as f64
    }
    fn swap_used_pct(&self) -> f64 {
        if self.swap_total_kb == 0 {
            return 0.0;
        }
        100.0 * (self.swap_total_kb.saturating_sub(self.swap_free_kb)) as f64
            / self.swap_total_kb as f64
    }
}

fn read_meminfo() -> Option<MemInfo> {
    let text = fs::read_to_string("/proc/meminfo").ok()?;
    let mut m: HashMap<&str, u64> = HashMap::new();
    for line in text.lines() {
        if let Some((k, v)) = line.split_once(':') {
            if let Some(n) = v.split_whitespace().next().and_then(|n| n.parse().ok()) {
                m.insert(k, n);
            }
        }
    }
    Some(MemInfo {
        total_kb: *m.get("MemTotal")?,
        available_kb: *m.get("MemAvailable")?,
        swap_total_kb: *m.get("SwapTotal").unwrap_or(&0),
        swap_free_kb: *m.get("SwapFree").unwrap_or(&0),
    })
}

/// Pressure Stall Information: percentuale di tempo in cui almeno un task
/// è rimasto bloccato in attesa di memoria (media sugli ultimi 10 s).
fn read_psi_some_avg10() -> Option<f64> {
    let text = fs::read_to_string("/proc/pressure/memory").ok()?;
    let line = text.lines().find(|l| l.starts_with("some"))?;
    line.split_whitespace()
        .find_map(|f| f.strip_prefix("avg10="))
        .and_then(|v| v.parse().ok())
}

fn mb(kb: u64) -> u64 {
    kb / 1024
}

// ---------------------------------------------------------------- processi

struct Proc {
    name: String,
    rss_kb: u64,
}

/// Elenca i processi dell'utente corrente, ordinati per RSS decrescente.
fn own_processes() -> Vec<Proc> {
    let uid = unsafe { libc::getuid() };
    let mut out = Vec::new();
    let Ok(dir) = fs::read_dir("/proc") else {
        return out;
    };
    for e in dir.flatten() {
        if !e.file_name().to_str().is_some_and(|s| s.bytes().all(|b| b.is_ascii_digit())) {
            continue;
        }
        let Ok(status) = fs::read_to_string(e.path().join("status")) else {
            continue;
        };
        let (mut name, mut rss, mut puid) = (String::new(), 0u64, u32::MAX);
        for l in status.lines() {
            if let Some(v) = l.strip_prefix("Name:") {
                name = v.trim().to_string();
            } else if let Some(v) = l.strip_prefix("VmRSS:") {
                rss = v.split_whitespace().next().and_then(|n| n.parse().ok()).unwrap_or(0);
            } else if let Some(v) = l.strip_prefix("Uid:") {
                puid = v.split_whitespace().next().and_then(|n| n.parse().ok()).unwrap_or(u32::MAX);
            }
        }
        if puid == uid && rss > 0 {
            out.push(Proc { name, rss_kb: rss });
        }
    }
    out.sort_by(|a, b| b.rss_kb.cmp(&a.rss_kb));
    out
}

// ---------------------------------------------------------------- pulizia

/// Cgroup v2 della sessione utente (user@UID.service), scrivibile senza root.
/// Il kernel recupera pagine (cache file + anonime verso swap) da tutta la
/// gerarchia, cioè da tutte le applicazioni KDE/GNOME dell'utente.
fn user_cgroup() -> Option<String> {
    let uid = unsafe { libc::getuid() };
    let candidates = [
        format!("/sys/fs/cgroup/user.slice/user-{uid}.slice/user@{uid}.service"),
        format!("/sys/fs/cgroup/user.slice/user-{uid}.slice"),
    ];
    candidates
        .into_iter()
        .find(|c| fs::metadata(format!("{c}/memory.reclaim")).is_ok())
}

fn cgroup_current_kb(cg: &str) -> u64 {
    fs::read_to_string(format!("{cg}/memory.current"))
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
        .map_or(0, |b| b / 1024)
}

/// Chiede al kernel di recuperare `target_kb` dal cgroup dell'utente.
/// Restituisce i KB effettivamente liberati. Il kernel risponde EAGAIN se non
/// riesce a recuperare tutto: si riprova con quantità dimezzate.
fn reclaim_cgroup(cg: &str, target_kb: u64) -> u64 {
    let before = cgroup_current_kb(cg);
    let mut ask = target_kb;
    while ask >= 64 * 1024 {
        if fs::write(format!("{cg}/memory.reclaim"), format!("{}K", ask)).is_ok() {
            break;
        }
        ask /= 2;
    }
    before.saturating_sub(cgroup_current_kb(cg))
}

/// Libera cache di sistema con privilegi, solo se l'utente l'ha chiesto
/// (--root-clean): richiede l'autenticazione grafica di polkit (KDE e GNOME
/// mostrano entrambi il proprio dialogo).
fn drop_caches_privileged() -> bool {
    Command::new("pkexec")
        .args(["sh", "-c", "sync; echo 1 > /proc/sys/vm/drop_caches"])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Libera memoria. Ritorna (KB liberati, descrizione del metodo).
fn free_memory(mem: &MemInfo, cfg: &Config) -> (u64, String) {
    let mut total = 0u64;
    let mut methods = Vec::new();

    if let Some(cg) = user_cgroup() {
        // Obiettivo: riportare la RAM usata sotto la soglia di avviso, ma
        // almeno 512 MB e al più metà della memoria del cgroup.
        let used_kb = mem.total_kb.saturating_sub(mem.available_kb);
        let goal_kb = used_kb.saturating_sub((mem.total_kb as f64 * cfg.warn_pct / 100.0) as u64);
        let cg_kb = cgroup_current_kb(&cg);
        let ask = goal_kb.max(512 * 1024).min(cg_kb / 2);
        let freed = reclaim_cgroup(&cg, ask);
        total += freed;
        methods.push(format!("recupero cgroup ({} MB)", mb(freed)));
    }

    if cfg.root_clean && drop_caches_privileged() {
        methods.push("cache di sistema svuotata".to_string());
    }

    if methods.is_empty() {
        methods.push("nessun metodo disponibile".to_string());
    }
    (total, methods.join(", "))
}

// ---------------------------------------------------------------- notifiche

/// Notifica desktop tramite org.freedesktop.Notifications (KDE e GNOME).
/// `replace_id` evita di accumulare popup: ogni notifica sostituisce la precedente.
fn notify(title: &str, body: &str, urgency: &str, icon: &str) {
    let _ = Command::new("notify-send")
        .args([
            "--app-name=memguard",
            &format!("--urgency={urgency}"),
            &format!("--icon={icon}"),
            "--hint=string:x-canonical-private-synchronous:memguard",
            title,
            body,
        ])
        .status();
}

fn top_consumers(n: usize) -> String {
    own_processes()
        .iter()
        .take(n)
        .map(|p| format!("{} {} MB", p.name, mb(p.rss_kb)))
        .collect::<Vec<_>>()
        .join(", ")
}

fn desktop_name() -> String {
    env::var("XDG_CURRENT_DESKTOP").unwrap_or_else(|_| "sconosciuto".into())
}

// ---------------------------------------------------------------- icona nella barra

#[derive(PartialEq, Clone, Copy, Debug)]
enum Level {
    Ok,
    Warn,
    Critical,
}

/// Stato mostrato dall'icona: verde = ok, giallo = verso la soglia, rosso = sulla soglia.
struct Tray {
    level: Level,
    used: f64,
    swap: f64,
    psi: f64,
    avail_mb: u64,
    total_mb: u64,
    clean_requested: Arc<AtomicBool>,
}

fn level_rgb(l: Level) -> (u8, u8, u8) {
    match l {
        Level::Ok => (0x2e, 0xcc, 0x40),
        Level::Warn => (0xff, 0xc1, 0x07),
        Level::Critical => (0xe7, 0x30, 0x2a),
    }
}

/// Disegna un cerchio pieno con bordo scuro e antialiasing, in formato ARGB32.
fn circle_icon(size: i32, (r, g, b): (u8, u8, u8)) -> ksni::Icon {
    let c = (size as f32 - 1.0) / 2.0;
    let radius = size as f32 / 2.0 - 1.0;
    let mut data = Vec::with_capacity((size * size * 4) as usize);
    for y in 0..size {
        for x in 0..size {
            let d = ((x as f32 - c).powi(2) + (y as f32 - c).powi(2)).sqrt();
            let alpha = (radius + 0.5 - d).clamp(0.0, 1.0);
            let border = d > radius - 1.6;
            let (pr, pg, pb) = if border { (r / 2, g / 2, b / 2) } else { (r, g, b) };
            data.extend_from_slice(&[(alpha * 255.0) as u8, pr, pg, pb]);
        }
    }
    ksni::Icon { width: size, height: size, data }
}

impl ksni::Tray for Tray {
    fn id(&self) -> String {
        "memguard".into()
    }
    fn title(&self) -> String {
        "memguard".into()
    }
    fn category(&self) -> ksni::Category {
        ksni::Category::SystemServices
    }
    fn icon_pixmap(&self) -> Vec<ksni::Icon> {
        let rgb = level_rgb(self.level);
        vec![circle_icon(22, rgb), circle_icon(32, rgb), circle_icon(48, rgb)]
    }
    fn tool_tip(&self) -> ksni::ToolTip {
        let stato = match self.level {
            Level::Ok => "Memoria nella norma",
            Level::Warn => "Memoria verso la soglia",
            Level::Critical => "Memoria sulla soglia critica",
        };
        ksni::ToolTip {
            title: format!("memguard: {stato}"),
            description: format!(
                "RAM {:.0}% ({} MB liberi di {} MB)\nSwap {:.0}%\nPressione {:.1}",
                self.used, self.avail_mb, self.total_mb, self.swap, self.psi
            ),
            ..Default::default()
        }
    }
    fn menu(&self) -> Vec<ksni::MenuItem<Self>> {
        use ksni::menu::*;
        let stato = match self.level {
            Level::Ok => "Stato: normale",
            Level::Warn => "Stato: attenzione",
            Level::Critical => "Stato: critico",
        };
        vec![
            StandardItem { label: stato.into(), enabled: false, ..Default::default() }.into(),
            StandardItem {
                label: format!("RAM {:.0}% · swap {:.0}% · PSI {:.1}", self.used, self.swap, self.psi),
                enabled: false,
                ..Default::default()
            }
            .into(),
            MenuItem::Separator,
            StandardItem {
                label: "Libera memoria ora".into(),
                icon_name: "edit-clear".into(),
                activate: Box::new(|t: &mut Tray| t.clean_requested.store(true, Ordering::Relaxed)),
                ..Default::default()
            }
            .into(),
            StandardItem {
                label: "Esci".into(),
                icon_name: "application-exit".into(),
                activate: Box::new(|_| std::process::exit(0)),
                ..Default::default()
            }
            .into(),
        ]
    }
}

// ---------------------------------------------------------------- main

fn main() {
    let cfg = parse_args();
    let has_psi = read_psi_some_avg10().is_some();

    println!(
        "memguard avviato (desktop: {}, PSI: {}, dry-run: {})",
        desktop_name(),
        if has_psi { "sì" } else { "no, uso solo %RAM" },
        cfg.dry_run
    );

    let clean_requested = Arc::new(AtomicBool::new(false));
    let tray = if cfg.no_tray || cfg.once {
        None
    } else {
        let t = Tray {
            level: Level::Ok,
            used: 0.0,
            swap: 0.0,
            psi: 0.0,
            avail_mb: 0,
            total_mb: 0,
            clean_requested: clean_requested.clone(),
        };
        match t.spawn() {
            Ok(h) => Some(h),
            Err(e) => {
                eprintln!("icona nella barra non disponibile: {e}");
                None
            }
        }
    };

    let mut last_level = Level::Ok;
    let mut last_warn: Option<Instant> = None;
    let mut last_clean: Option<Instant> = None;

    loop {
        let Some(mem) = read_meminfo() else {
            eprintln!("impossibile leggere /proc/meminfo");
            sleep(cfg.interval);
            continue;
        };
        let psi = read_psi_some_avg10().unwrap_or(0.0);
        let used = mem.used_pct();
        let swap = mem.swap_used_pct();

        // Critico: rallentamento imminente. Il PSI è il segnale più diretto,
        // la % RAM + swap alta è il segnale anticipatore.
        let level = if used >= cfg.critical_pct
            || psi >= cfg.psi_critical
            || (used >= cfg.warn_pct && swap >= cfg.swap_critical)
        {
            Level::Critical
        } else if used >= cfg.warn_pct * 0.9 || psi >= cfg.psi_warn * 0.5 {
            Level::Warn
        } else {
            Level::Ok
        };

        println!(
            "[{:?}] RAM {:.1}% ({} MB liberi di {} MB) | swap {:.1}% | PSI {:.2}",
            level,
            used,
            mb(mem.available_kb),
            mb(mem.total_kb),
            swap,
            psi
        );

        if let Some(h) = &tray {
            let (avail, total) = (mb(mem.available_kb), mb(mem.total_kb));
            let _ = h.update(move |t: &mut Tray| {
                t.level = level;
                t.used = used;
                t.swap = swap;
                t.psi = psi;
                t.avail_mb = avail;
                t.total_mb = total;
            });
        }

        if clean_requested.swap(false, Ordering::Relaxed) {
            let (freed, method) = free_memory(&mem, &cfg);
            let after = read_meminfo().unwrap_or(mem);
            notify(
                "Pulizia manuale eseguita",
                &format!("Liberati circa {} MB ({}). RAM ora {:.0}%.", mb(freed), method, after.used_pct()),
                "normal",
                "dialog-information",
            );
        }

        let ready = |t: Option<Instant>| t.map_or(true, |t| t.elapsed() >= cfg.cooldown);

        match level {
            Level::Warn
                if (used >= cfg.warn_pct || psi >= cfg.psi_warn)
                    && (last_level == Level::Ok || ready(last_warn)) =>
            {
                notify(
                    "Memoria quasi satura",
                    &format!(
                        "RAM usata {:.0}% ({} MB disponibili), pressione {:.1}.\nPrincipali: {}",
                        used,
                        mb(mem.available_kb),
                        psi,
                        top_consumers(3)
                    ),
                    "normal",
                    "dialog-warning",
                );
                last_warn = Some(Instant::now());
            }
            Level::Critical if ready(last_clean) => {
                if cfg.dry_run {
                    notify(
                        "Memoria satura (dry-run)",
                        &format!("RAM {:.0}%, pressione {:.1}. Nessuna pulizia eseguita.\n{}", used, psi, top_consumers(3)),
                        "critical",
                        "dialog-error",
                    );
                } else {
                    let (freed, method) = free_memory(&mem, &cfg);
                    let after = read_meminfo().unwrap_or(mem);
                    let body = if freed > 0 {
                        format!(
                            "Liberati circa {} MB ({}). RAM ora {:.0}% (era {:.0}%).",
                            mb(freed),
                            method,
                            after.used_pct(),
                            used
                        )
                    } else {
                        format!(
                            "RAM {:.0}%, pressione {:.1}. Poca memoria recuperabile automaticamente: chiudi qualche applicazione.\nPrincipali: {}",
                            used,
                            psi,
                            top_consumers(3)
                        )
                    };
                    notify("Memoria satura: pulizia eseguita", &body, "critical", "dialog-information");
                }
                last_clean = Some(Instant::now());
                last_warn = Some(Instant::now());
            }
            Level::Ok if last_level != Level::Ok => {
                notify(
                    "Memoria nella norma",
                    &format!("RAM usata {:.0}%, {} MB disponibili.", used, mb(mem.available_kb)),
                    "low",
                    "dialog-ok",
                );
            }
            _ => {}
        }
        last_level = level;

        if cfg.once {
            break;
        }
        sleep(cfg.interval);
    }
}
