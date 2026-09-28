// Sorgアプリ用スキャナ:  CLI(sorg)と同じ判定パイプラインをTauriコマンドとして公開する
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use anyhow::Result;
use reqwest::blocking::Client;
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tauri::Emitter;
use walkdir::WalkDir;

// ---------- 設定 ----------

#[derive(Serialize, Deserialize, Clone)]
pub struct Config {
    pub endpoint: String,
    pub model: String,
    pub min_confidence: f64,
    pub archive_root: Option<String>,
    pub categories: HashMap<String, String>,
}

impl Default for Config {
    fn default() -> Self {
        let mut categories = HashMap::new();
        categories.insert("work".into(), "仕事・業務・会議・チャット・Slack/Teamsの画面".into());
        categories.insert("finance".into(), "請求書・領収書・支払い・経理・銀行".into());
        categories.insert("tech".into(), "エラー画面・コード・ターミナル・開発ツール".into());
        categories.insert("personal".into(), "私的なやり取り・買い物・SNS・趣味".into());
        categories.insert("reference".into(), "後で見るための資料・記事・メモのスクショ".into());
        categories.insert("entertainment".into(), "動画・ゲーム・エンタメの画面".into());
        Config {
            endpoint: "http://localhost:11435/v1/systemone".into(),
            model: "laya:multilingual".into(),
            min_confidence: 0.60,
            archive_root: None,
            categories,
        }
    }
}

fn home() -> PathBuf {
    std::env::var("HOME").unwrap_or_else(|_| "/tmp".into()).into()
}

fn config_dir() -> PathBuf {
    let d = home().join(".sorg");
    let _ = std::fs::create_dir_all(&d);
    d
}

fn load_config() -> Config {
    let p = config_dir().join("config.json");
    std::fs::read_to_string(&p)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn save_config(cfg: &Config) -> Result<(), String> {
    let p = config_dir().join("config.json");
    std::fs::write(&p, serde_json::to_string_pretty(cfg).map_err(|e| e.to_string())?)
        .map_err(|e| e.to_string())
}

// ---------- 表示用構造体 ----------

#[derive(Clone, Serialize)]
struct ShotInfo {
    id: String, // ハッシュ
    path: String,
    name: String,
    category: Option<String>,
    confidence: Option<f64>,
    keep: Option<f64>,
    status: String, // pending | classified | no-text | error
    mtime: String,
    size_kb: u64,
}

#[derive(Clone, Serialize)]
struct Progress {
    done: usize,
    total: usize,
    current: String,
}

// ---------- キャッシュ ----------

struct Store {
    conn: Connection,
}

impl Store {
    fn open() -> Result<Self> {
        let conn = Connection::open(config_dir().join("cache.db"))?;
        conn.execute(
            "CREATE TABLE IF NOT EXISTS seen (
                hash TEXT PRIMARY KEY,
                path TEXT NOT NULL,
                category TEXT NOT NULL,
                keep REAL NOT NULL,
                decided_at TEXT NOT NULL
            )",
            [],
        )?;
        let s = Self { conn };
        s.ensure_confidence();
        Ok(s)
    }

    fn lookup(&self, hash: &str) -> Option<(String, f64, f64)> {
        // confidence カラムは CLI 版との共有で古い行には無いので COALESCE
        self.conn
            .query_row(
                "SELECT category, COALESCE(confidence, 1.0), keep FROM seen WHERE hash = ?1",
                [hash],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .ok()
    }

    fn has_column(&self, name: &str) -> bool {
        let mut st = match self.conn.prepare("PRAGMA table_info(seen)") {
            Ok(s) => s,
            Err(_) => return false,
        };
        let cols: Vec<String> = match st.query_map([], |r| r.get::<_, String>(1)) {
            Ok(rows) => rows.flatten().collect(),
            Err(_) => return false,
        };
        cols.iter().any(|c| c == name)
    }

    fn ensure_confidence(&self) {
        if !self.has_column("confidence") {
            let _ = self.conn.execute(
                "ALTER TABLE seen ADD COLUMN confidence REAL NOT NULL DEFAULT 1.0",
                [],
            );
        }
    }

    fn store(&self, hash: &str, path: &Path, category: &str, conf: f64, keep: f64) {
        let _ = self.conn.execute(
            "INSERT OR REPLACE INTO seen (hash, path, category, confidence, keep, decided_at)
             VALUES (?1, ?2, ?3, ?4, ?5, datetime('now'))",
            rusqlite::params![hash, path.to_string_lossy(), category, conf, keep],
        );
    }
}

fn sha256_file(path: &Path) -> Option<String> {
    let bytes = std::fs::read(path).ok()?;
    let mut h = Sha256::new();
    h.update(&bytes);
    Some(hex::encode(h.finalize()))
}

// ---------- Ollaya ----------

#[derive(Serialize)]
#[serde(untagged)]
enum Criteria {
    Choice(HashMap<String, String>),
    Score(Vec<String>),
}

#[derive(Serialize)]
struct Question {
    #[serde(rename = "type")]
    qtype: String,
    instructions: String,
    criteria: Criteria,
}

#[derive(Serialize)]
struct OllayaRequest {
    model: String,
    state: String,
    questions: HashMap<String, Question>,
}

#[derive(Deserialize)]
struct Answers {
    choice: Option<String>,
    confidence: Option<f64>,
    score: Option<f64>,
}

#[derive(Deserialize)]
struct Decision {
    #[serde(default)]
    answers: HashMap<String, Answers>,
}

fn classify(client: &Client, cfg: &Config, text: &str) -> Result<(String, f64, f64)> {
    let mut criteria = cfg.categories.clone();
    criteria.insert("other".into(), "上記のどれにも当てはまらない".into());

    let mut questions = HashMap::new();
    questions.insert(
        "category".into(),
        Question {
            qtype: "choice".into(),
            instructions: "このスクリーンショットに写っている内容のカテゴリは?".into(),
            criteria: Criteria::Choice(criteria),
        },
    );
    questions.insert(
        "keep".into(),
        Question {
            qtype: "score".into(),
            instructions: "このスクリーンショットはあとで参照する保存価値があるか".into(),
            criteria: Criteria::Score(vec!["低".into(), "中".into(), "高".into()]),
        },
    );

    let state: String = text.chars().take(500).collect();
    let req = OllayaRequest {
        model: cfg.model.clone(),
        state,
        questions,
    };
    let resp: Decision = client
        .post(&cfg.endpoint)
        .json(&req)
        .send()?
        .error_for_status()?
        .json()?;

    let a = resp.answers.get("category");
    let category = a
        .and_then(|a| a.choice.clone())
        .unwrap_or_else(|| "unclassified".into());
    let conf = a.and_then(|a| a.confidence).unwrap_or(0.0);
    let keep = (resp
        .answers
        .get("keep")
        .and_then(|k| k.score)
        .unwrap_or(1.0)
        / 2.0)
        .clamp(0.0, 1.0);
    Ok((category, conf, keep))
}

// ---------- ファイル走査 ----------

const IMAGE_EXTS: &[&str] = &["png", "jpg", "jpeg", "heic", "tiff", "gif"];

fn is_screenshot(path: &Path) -> bool {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    let looks = name.contains("スクリーンショット")
        || name.contains("スクショ")
        || name.starts_with("screenshot")
        || name.starts_with("clean shot");
    let is_img = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| IMAGE_EXTS.contains(&e.to_lowercase().as_str()))
        .unwrap_or(false);
    looks && is_img
}

fn scan_dir(dir: &Path) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = WalkDir::new(dir)
        .max_depth(2)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file() && is_screenshot(e.path()))
        .map(|e| e.path().to_path_buf())
        .collect();
    files.sort();
    files
}

fn ocr(path: &Path) -> Result<String> {
    use apple_vision::prelude::*;
    let recognizer = TextRecognizer::new()
        .with_recognition_level(apple_vision::RecognitionLevel::Accurate)
        .with_language_correction(false);
    let obs = recognizer.recognize_in_path(path)?;
    Ok(obs.iter().map(|o| o.text.clone()).collect::<Vec<_>>().join("\n"))
}

// ---------- Tauriコマンド ----------

#[tauri::command]
fn list_shots(dir: Option<String>) -> Result<Vec<ShotInfo>, String> {
    let store = Store::open().map_err(|e| e.to_string())?;
    let root = match dir {
        Some(d) => PathBuf::from(d),
        None => home().join("Desktop"),
    };
    let files = scan_dir_files(&root).into_iter().map(|(p, _)| p).collect::<Vec<_>>();
    let mut out = Vec::new();
    for f in files {
        let Some(hash) = sha256_file(&f) else { continue };
        let meta = std::fs::metadata(&f).ok();
        let (category, confidence, keep, status) = match store.lookup(&hash) {
            Some((c, cf, k)) => (Some(c), Some(cf), Some(k), "classified".to_string()),
            None => (None, None, None, "pending".to_string()),
        };
        out.push(ShotInfo {
            id: hash,
            path: f.to_string_lossy().into_owned(),
            name: f.file_name().unwrap_or_default().to_string_lossy().into_owned(),
            category,
            confidence,
            keep,
            status,
            mtime: meta
                .as_ref()
                .and_then(|m| m.modified().ok())
                .map(|t| {
                    let d: std::time::SystemTime = t;
                    let secs = d
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|x| x.as_secs())
                        .unwrap_or(0);
                    format!("{secs}")
                })
                .unwrap_or_default(),
            size_kb: meta.map(|m| m.len() / 1024).unwrap_or(0),
        });
    }
    Ok(out)
}

// 判定ステータス(JS側が invoke で直接引く:イベント配信に依存しない)
#[derive(Clone, Serialize)]
struct ClassifyStats {
    running: bool,
    done: usize,
    total: usize,
    current: String,
    last_error: String,
}

static CLASSIFY: std::sync::OnceLock<
    std::sync::Mutex<ClassifyStats>,
> = std::sync::OnceLock::new();

fn stats() -> &'static std::sync::Mutex<ClassifyStats> {
    CLASSIFY.get_or_init(|| {
        std::sync::Mutex::new(ClassifyStats {
            running: false,
            done: 0,
            total: 0,
            current: String::new(),
            last_error: String::new(),
        })
    })
}

fn set_stats(f: impl FnOnce(&mut ClassifyStats)) {
    if let Ok(mut s) = stats().lock() {
        f(&mut s);
    }
}

#[tauri::command]
fn classify_stats() -> Result<ClassifyStats, String> {
    Ok(stats().lock().map_err(|e| e.to_string())?.clone())
}

#[tauri::command]
fn classify_pending(app: tauri::AppHandle, dir: Option<String>, force: Option<bool>) -> Result<usize, String> {
    let force = force.unwrap_or(false);
    let cfg = load_config();
    let store = Store::open().map_err(|e| e.to_string())?;
    let client = Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .map_err(|e| e.to_string())?;
    let root = match dir {
        Some(d) => PathBuf::from(d),
        None => home().join("Desktop"),
    };
    let files = scan_dir_files(&root).into_iter().map(|(p, _)| p).collect::<Vec<_>>();
    let mut targets: Vec<(PathBuf, String)> = Vec::new();
    for f in &files {
        let Some(h) = sha256_file(f) else { continue };
        // force: 全ファイルを再判定(キャッシュがあっても再OCR・上書き保存)
        if force || store.lookup(&h).is_none() {
            targets.push((f.clone(), h));
        }
    }
    let total = targets.len();
    set_stats(|s| {
        s.running = true;
        s.done = 0;
        s.total = total;
        s.current = String::new();
        s.last_error = String::new();
    });
    // 別スレッドでOCR+判定。進捗は CLASSIFY ミューテックスに記録する(JSが引く)
    std::thread::spawn(move || {
        let mut failed = 0usize;
        for (f, hash) in &targets {
            let name = f
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned();
            set_stats(|s| {
                s.current = name.clone();
            });
            let _ = app.emit("progress", ());
            let text = ocr(f).unwrap_or_default();
            let (category, conf, keep) = if text.trim().is_empty() {
                ("no-text".to_string(), 0.0, 0.5)
            } else {
                match classify(&client, &cfg, &text) {
                    Ok(v) => v,
                    Err(e) => {
                        failed += 1;
                        let msg = format!("{}: {e}", f.display());
                        eprintln!("判定失敗 {msg}");
                        set_stats(|s| s.last_error = msg);
                        set_stats(|s| s.done += 1);
                        continue;
                    }
                }
            };
            store.store(hash, f, &category, conf, keep);
            set_stats(|s| s.done += 1);
        }
        set_stats(|s| {
            s.running = false;
            if failed > 0 {
                s.last_error = format!("{failed} 件判定失敗");
            }
        });
        let _ = app.emit("done", ());
    });
    Ok(total)
}

#[derive(Clone, Serialize)]
struct CategoryInfo {
    name: String,
    criteria: String,
    count: usize, // Pictures/<cat> 内のスクショ数
}

#[tauri::command]
fn list_categories() -> Result<Vec<CategoryInfo>, String> {
    let cfg = load_config();
    let root = archive_root(&cfg);
    let mut out = Vec::new();
    // 設定上のカテゴリ + 実際にPicturesに存在するフォルダの和集合
    let mut names: std::collections::BTreeSet<String> = cfg.categories.keys().cloned().collect();
    if let Ok(rd) = std::fs::read_dir(&root) {
        for e in rd.flatten() {
            if e.path().is_dir() {
                if let Some(n) = e.path().file_name().map(|n| n.to_string_lossy().into_owned()) {
                    names.insert(n);
                }
            }
        }
    }
    for n in names {
        let dir = root.join(&n);
        let count = if dir.is_dir() {
            scan_dir_files(&dir).len()
        } else {
            0
        };
        out.push(CategoryInfo {
            criteria: cfg
                .categories
                .get(&n)
                .cloned()
                .unwrap_or_else(|| "(カスタムカテゴリ: 判定基準の設定が必要です)".into()),
            name: n,
            count,
        });
    }
    Ok(out)
}

#[tauri::command]
fn add_category(name: String, criteria: String) -> Result<(), String> {
    let mut cfg = load_config();
    let name = name.trim().to_string();
    if name.is_empty() || name.contains('/') {
        return Err("カテゴリ名に / は使えません".into());
    }
    cfg.categories.insert(name.clone(), criteria);
    save_config(&cfg)?;
    std::fs::create_dir_all(archive_root(&cfg).join(&name)).map_err(|e| e.to_string())?;
    Ok(())
}

#[tauri::command]
fn delete_category(name: String, return_files: bool) -> Result<usize, String> {
    let cfg = load_config();
    let dir = archive_root(&cfg).join(&name);
    let mut moved = 0;
    if dir.is_dir() {
        if return_files {
            // カテゴリ内のスクショを Desktop に戻す(名前はそのまま、衝突時は連番を振る)
            for (f, _ext) in scan_dir_files(&dir) {
                let base = f
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .to_string();
                let dest = unique_path(&home().join("Desktop"), &base);
                std::fs::rename(&f, &dest)
                    .or_else(|_| {
                        std::fs::copy(&f, &dest)?;
                        std::fs::remove_file(&f)
                    })
                    .map_err(|e| format!("{}: {e}", f.display()))?;
                moved += 1;
            }
        }
        // ファイルが残っているなら削除しない(空になるか、明示的にreturn_files=falseなら残す)
        if dir_exists_empty(&dir) {
            let _ = std::fs::remove_dir(&dir);
        }
    }
    let mut cfg2 = load_config();
    cfg2.categories.remove(&name);
    save_config(&cfg2)?;
    Ok(moved)
}

fn dir_exists_empty(dir: &Path) -> bool {
    match std::fs::read_dir(dir) {
        Ok(mut rd) => rd.next().is_none(),
        Err(_) => false,
    }
}

fn unique_path(dir: &Path, name: &str) -> PathBuf {
    let mut dest = dir.join(name);
    if !dest.exists() {
        return dest;
    }
    let p = Path::new(name);
    let stem = p.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
    let ext = p.extension().map(|s| s.to_string_lossy().to_string());
    for i in 2..10000 {
        dest = match &ext {
            Some(e) => dir.join(format!("{stem}-{i}.{e}")),
            None => dir.join(format!("{stem}-{i}")),
        };
        if !dest.exists() {
            return dest;
        }
    }
    dest
}

/// scan_dirと同じファイル集合を拡張子つきで返す(整理・カテゴリ削除用)
fn scan_dir_files(dir: &Path) -> Vec<(PathBuf, String)> {
    let mut files = Vec::new();
    for e in WalkDir::new(dir).max_depth(1).into_iter().filter_map(|e| e.ok()) {
        let f = e.path();
        if !f.is_file() {
            continue;
        }
        let name = f
            .file_name()
            .map(|n| n.to_string_lossy().to_lowercase())
            .unwrap_or_default();
        let is_img = f
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| IMAGE_EXTS.contains(&e.to_lowercase().as_str()))
            .unwrap_or(false);
        // 戻す対象は「スクショ名 + 画像拡張子」に限定し、他のフォルダ(整理済み等)に触らない
        let looks = name.contains("スクリーンショット")
            || name.contains("スクショ")
            || name.starts_with("screenshot")
            || name.starts_with("clean shot");
        if looks && is_img {
            files.push((f.to_path_buf(), name));
        }
    }
    files
}

fn archive_root(cfg: &Config) -> PathBuf {
    match &cfg.archive_root {
        Some(r) => PathBuf::from(r),
        None => home().join("Pictures"),
    }
}

#[tauri::command]
fn organize(files: Vec<String>, category: String) -> Result<usize, String> {
    let cfg = load_config();
    let root = archive_root(&cfg);
    std::fs::create_dir_all(&root).map_err(|e| e.to_string())?;
    let mut moved = 0;
    for f in &files {
        let src = Path::new(f);
        if !src.exists() {
            continue;
        }
        let dest_dir = root.join(&category);
        std::fs::create_dir_all(&dest_dir).map_err(|e| e.to_string())?;
        let dest = unique_path(&dest_dir, src.file_name().unwrap_or_default().to_string_lossy().as_ref());
        std::fs::rename(src, &dest)
            .or_else(|_| {
                std::fs::copy(src, &dest)?;
                std::fs::remove_file(src)
            })
            .map_err(|e| format!("{f}: {e}"))?;
        moved += 1;
    }
    Ok(moved)
}

#[tauri::command]
fn trash(files: Vec<String>) -> Result<usize, String> {
    // ~/.Trash へ退避(完全削除しない)
    let trash = home().join(".Trash");
    let mut moved = 0;
    for f in &files {
        let src = Path::new(f);
        if !src.exists() {
            continue;
        }
        let name = src.file_name().unwrap_or_default().to_string_lossy().to_string();
        let mut dest = trash.join(&name);
        if dest.exists() {
            dest = trash.join(format!(
                "{}-{}",
                name.trim_end_matches(".png"),
                chrono_like_stamp()
            ));
        }
        std::fs::rename(src, &dest)
            .or_else(|_| {
                std::fs::copy(src, &dest)?;
                std::fs::remove_file(src)
            })
            .map_err(|e| format!("{f}: {e}"))?;
        moved += 1;
    }
    Ok(moved)
}

fn chrono_like_stamp() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .setup(|app| {
            use tauri::Manager;
            if let Some(w) = app.get_webview_window("main") {
                let _ = w.set_always_on_top(true);
                let _ = w.show();
                let _ = w.set_focus();
                let _ = w.center();
                // 3秒後に最前面固定を解除(じゃまにならないように)
                let w2 = w.clone();
                std::thread::spawn(move || {
                    std::thread::sleep(std::time::Duration::from_secs(3));
                    let _ = w2.set_always_on_top(false);
                });
            }
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            list_shots, classify_pending, classify_stats, organize, trash,
            list_categories, add_category, delete_category
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
