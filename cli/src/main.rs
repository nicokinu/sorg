use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command as ProcessCommand;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use reqwest::blocking::Client;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use walkdir::WalkDir;

// ---------- CLI ----------

#[derive(Parser)]
#[command(name = "sorg", about = "スクリーンショットをOllayaで自動分類する")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// 整理案を表示するだけ(实际の移動はしない)
    Plan {
        /// 対象ディレクトリ(デフォルト: ~/Desktop)
        path: Option<PathBuf>,
    },
    /// 整理を実行する(ファイル移動)
    Apply {
        path: Option<PathBuf>,
        /// 低信頼のものを「未分類」に入れない
        #[arg(long)]
        strict: bool,
    },
    /// 分類カテゴリやOCRの設定を初期化する
    Init,
}

// ---------- 設定 ----------

#[derive(Serialize, Deserialize, Clone)]
struct Config {
    /// Ollaya サーバー
    endpoint: String,
    model: String,
    /// 信頼度がこの値未満なら「未分類」
    min_confidence: f64,
    /// 整理先を作成するルート(デフォルト: ~/Pictures/Screenshots-organized)
    archive_root: Option<String>,
    categories: HashMap<String, String>,
}

impl Default for Config {
    fn default() -> Self {
        let mut categories: HashMap<String, String> = HashMap::new();
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

fn config_dir() -> Result<PathBuf> {
    let dir = dirs_home()?.join(".sorg");
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

fn dirs_home() -> Result<PathBuf> {
    Ok(std::env::var("HOME").context("HOME環境変数がありません")?.into())
}

fn load_config() -> Result<Config> {
    let path = config_dir()?.join("config.json");
    if path.exists() {
        let s = std::fs::read_to_string(&path)?;
        Ok(serde_json::from_str(&s)?)
    } else {
        let cfg = Config::default();
        std::fs::write(&path, serde_json::to_string_pretty(&cfg)?)?;
        eprintln!("設定を作成しました: {}", path.display());
        Ok(cfg)
    }
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
    #[allow(dead_code)]
    legend: Option<serde_json::Value>,
}

#[derive(Deserialize)]
struct Decision {
    #[serde(default)]
    answers: HashMap<String, Answers>,
}

struct Ollaya {
    client: Client,
    cfg: Config,
}

impl Ollaya {
    fn new(cfg: Config) -> Self {
        Self {
            client: Client::builder()
                .timeout(std::time::Duration::from_secs(10))
                .build()
                .unwrap(),
            cfg,
        }
    }

    /// OCRテキストを投げて (category, keep_score) を返す
    fn classify(&self, text: &str) -> Result<(String, f64, f64)> {
        let mut criteria = self.cfg.categories.clone();
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

        // OCRテキストが長い場合があるので先頭500文字に切る
        let state: String = text.chars().take(500).collect();

        let req = OllayaRequest {
            model: self.cfg.model.clone(),
            state,
            questions,
        };
        let resp: Decision = self
            .client
            .post(&self.cfg.endpoint)
            .json(&req)
            .send()
            .context("Ollaya サーバーに接続できませんでした (ollaya serve?)")?
            .error_for_status()?
            .json()?;

        let a = resp
            .answers
            .get("category")
            .context("answers.category がありません")?;
        let category = a
            .choice
            .clone()
            .unwrap_or_else(|| "unclassified".into());
        let conf = a.confidence.unwrap_or(0.0);
        let mut keep = resp
            .answers
            .get("keep")
            .and_then(|k| k.score)
            .unwrap_or(1.0)
            / 2.0; // scoreは0..2のレベル目盛り(低/中/高) → 0..1に正規化
        keep = keep.clamp(0.0, 1.0);
        Ok((category, conf, keep))
    }
}

// ---------- SQLite キャッシュ ----------

struct Cache {
    conn: rusqlite::Connection,
}

impl Cache {
    fn open() -> Result<Self> {
        let conn =
            rusqlite::Connection::open(config_dir()?.join("cache.db"))?;
        conn.execute(
            "CREATE TABLE IF NOT EXISTS seen (
                hash TEXT PRIMARY KEY,
                path TEXT NOT NULL,
                category TEXT NOT NULL,
                confidence REAL,
                keep REAL NOT NULL,
                decided_at TEXT NOT NULL
            )",
            [],
        )?;
        Ok(Self { conn })
    }

    fn lookup(&self, hash: &str) -> Option<(String, f64, f64)> {
        self.conn
            .query_row(
                "SELECT category, COALESCE(confidence, 1.0), keep FROM seen WHERE hash = ?1",
                [hash],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .ok()
    }

    fn store(&self, hash: &str, path: &Path, category: &str, conf: f64, keep: f64) {
        let _ = self.conn.execute(
            "INSERT OR REPLACE INTO seen (hash, path, category, confidence, keep, decided_at)
             VALUES (?1, ?2, ?3, ?4, ?5, datetime('now'))",
            rusqlite::params![hash, path.to_string_lossy(), category, conf, keep],
        );
    }
}

fn sha256_file(path: &Path) -> Result<String> {
    let bytes = std::fs::read(path)?;
    let mut h = Sha256::new();
    h.update(&bytes);
    Ok(hex::encode(h.finalize()))
}

// ---------- 走査 ----------

const IMAGE_EXTS: &[&str] = &["png", "jpg", "jpeg", "heic", "tiff", "gif"];

fn is_screenshot(path: &Path) -> bool {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    // 「スクリーンショット」「Screenshot」「スクショ」など、あるいはイメージ拡張子なら拾う
    let looks_like_shot = name.contains("スクリーンショット")
        || name.contains("スクショ")
        || name.starts_with("screenshot")
        || name.starts_with("clean shot")
        || name.starts_with("recorded");
    let is_image = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| IMAGE_EXTS.contains(&e.to_lowercase().as_str()))
        .unwrap_or(false);
    looks_like_shot && is_image
}

fn collect(dir: &Path) -> Vec<PathBuf> {
    WalkDir::new(dir)
        .max_depth(2)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file() && is_screenshot(e.path()))
        .map(|e| e.path().to_path_buf())
        .collect()
}

// ---------- OCR ----------

fn ocr(path: &Path) -> Result<String> {
    use apple_vision::prelude::*;
    let recognizer = TextRecognizer::new()
        .with_recognition_level(apple_vision::RecognitionLevel::Accurate)
        .with_language_correction(false);
    let obs = recognizer
        .recognize_in_path(path)
        .map_err(|e| anyhow::anyhow!("OCR失敗: {e}"))?;
    let mut lines: Vec<String> = Vec::new();
    for o in &obs {
        lines.push(o.text.clone());
    }
    Ok(lines.join("\n"))
}

// ---------- メイン ----------

struct Verdict {
    path: PathBuf,
    category: String,
    keep: f64,
    confidence: f64,
    from_cache: bool,
}

fn run_plan(path: Option<PathBuf>) -> Result<Vec<Verdict>> {
    let cfg = load_config()?;
    let cache = Cache::open()?;
    let ollaya = Ollaya::new(cfg.clone());
    let dir = path.unwrap_or_else(|| dirs_home().unwrap().join("Desktop"));

    let files = collect(&dir);
    if files.is_empty() {
        eprintln!("{} にスクリーンショットは見つかりませんでした", dir.display());
    }

    let mut verdicts = Vec::new();
    let t0 = std::time::Instant::now();
    for (i, f) in files.iter().enumerate() {
        let hash = sha256_file(f)?;
        if let Some((category, conf, keep)) = cache.lookup(&hash) {
            verdicts.push(Verdict {
                path: f.clone(),
                category,
                keep,
                confidence: conf,
                from_cache: true,
            });
            continue;
        }
        print!("  [{}/{}] OCR & 判定: {} ... ", i + 1, files.len(), f.file_name().unwrap().to_string_lossy());
        let text = ocr(f).unwrap_or_default();
        if text.trim().is_empty() {
            println!("(テキストなし → 画像のみ)");
            verdicts.push(Verdict {
                path: f.clone(),
                category: "no-text".into(),
                keep: 0.5,
                confidence: 0.0,
                from_cache: false,
            });
            continue;
        }
        let (category, conf, keep) = ollaya.classify(&text)?;
        println!("{category} (conf {:.2}, keep {:.2})", conf, keep);
        cache.store(&hash, f, &category, conf, keep);
        verdicts.push(Verdict {
            path: f.clone(),
            category,
            keep,
            confidence: conf,
            from_cache: false,
        });
    }
    eprintln!(
        "\n{} 枚、判定所要 {:.1}s",
        files.len(),
        t0.elapsed().as_secs_f64()
    );
    Ok(verdicts)
}

fn apply(verdicts: Vec<Verdict>, cfg: &Config, strict: bool) -> Result<()> {
    let root = match &cfg.archive_root {
        Some(r) => PathBuf::from(r),
        None => dirs_home()?.join("Pictures/Screenshots-organized"),
    };
    std::fs::create_dir_all(&root)?;

    let mut moved = 0;
    for v in verdicts {
        if v.category == "no-text" {
            continue; // 画像のみのスクショは触らない(当面)
        }
        if strict && v.confidence < cfg.min_confidence {
            eprintln!("  スキップ(低信頼): {} {}", v.category, v.path.display());
            continue;
        }
        let dest_dir = root.join(&v.category);
        std::fs::create_dir_all(&dest_dir)?;
        let name = v.path.file_name().unwrap();
        let dest = dest_dir.join(name);
        if dest.exists() {
            continue;
        }
        std::fs::rename(&v.path, &dest).or_else(|_| {
            // 別ボリュームの場合は copy+remove
            std::fs::copy(&v.path, &dest)?;
            std::fs::remove_file(&v.path)
        })?;
        moved += 1;
        println!("  {} → {}", v.path.display(), dest.display());
    }
    println!("{moved} 枚を整理しました");
    let _ = ProcessCommand::new("osascript")
        .args(["-e", "display notification \"スクショ整理完了\" with title \"sorg\""])
        .status();
    Ok(())
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Init => {
            let p = config_dir()?.join("config.json");
            load_config()?;
            println!("設定: {}", p.display());
            println!("キャッシュ: {}", config_dir()?.join("cache.db").display());
        }
        Command::Plan { path } => {
            let vs = run_plan(path)?;
            let mut by_cat: std::collections::BTreeMap<&str, usize> = std::collections::BTreeMap::new();
            for v in &vs {
                *by_cat.entry(v.category.as_str()).or_default() += 1;
            }
            println!("\n--- 内訳 ---");
            for (cat, n) in by_cat.iter().rev() {
                println!("{cat}: {n} 枚");
            }
            let low = vs.iter().filter(|v| v.confidence < 0.6).count();
            if low > 0 {
                println!("(うち {low} 枚は信頼度0.6未満 → Apply時に --strict ならスキップ)");
            }
        }
        Command::Apply { path, strict } => {
            let cfg = load_config()?;
            let vs = run_plan(path)?;
            apply(vs, &cfg, strict)?;
        }
    }
    Ok(())
}
