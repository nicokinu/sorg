# Sorg — スクショ整理(macOS)

デスクトップのスクリーンショットを、**ローカルのOCR + 判断モデル**でカテゴリ分類して
`~/Pictures/<category>/` に整理するツール集。すべてローカル完結で、外部APIには送信しません。

```
スクショ走査 (walkdir)
  → Apple Vision OCR (apple-vision crate / macOS内蔵・瞬時)
  → Ollaya laya:multilingual に choice(カテゴリ) + score(保存価値) を質問 (~10ms/枚)
  → ~/.sorg/cache.db にハッシュ単位キャッシュ
  → UI(or CLI)で選択 → ~/Pictures/<category>/ へ移動
```

## 構成

| ディレクトリ | 内容 |
|---|---|
| `app/` | Tauri 2 デスクトップアプリ(サムネイル一覧・一括選択・カテゴリ管理) |
| `cli/` | `sorg` CLI(`plan` / `apply --strict` / `init`) |

アプリとCLIは `~/.sorg/`(`config.json` と `cache.db`)を共有します。

## 必要環境

- macOS 14+ (Apple Vision OCR)
- [Ollaya](https://ollaya.dev) が `localhost:11435` で起動済み(`ollaya run laya:multilingual`)
- Rust、Node.js(アプリ)、CLI は Rust のみ

## アプリ

```bash
cd app
npm install
npx tauri dev      # 開発実行
npx tauri build    # .app / .dmg
```

操作:「未分類を判定」→ チップバーでフィルタ・「tech を全选択」→「選択を整理」。
カテゴリは「＋ カテゴリ追加」で判定基準つきで追加、「× 名前」で削除
(そのカテゴリに整理済みのファイルはDesktopへ戻してから削除します)。
カードの **⌘クリック** で同一カテゴリを一括選択。

## CLI

```bash
cd cli && cargo run -- plan          # 整理案を表示(移動しない)
cargo run -- apply --strict          # 低信頼を避けて ~/Pictures/<category> へ移動
```

## 設定 (~/.sorg/config.json)

```json
{
  "endpoint": "http://localhost:11435/v1/systemone",
  "model": "laya:multilingual",
  "min_confidence": 0.6,
  "archive_root": "/Users/<you>/Pictures",
  "categories": { "tech": "エラー画面・コード・ターミナル・開発ツール", ... }
}
```

カテゴリの説明(criteria)を書き換えて「全再判定」を押すと、全ファイルが新しい基準で再分類されます。

## 制限

- 画像のみのスクショ(動画サムネ等)は OCR 対象外(`no-text` のまま)→ vision モデルでの分類は未実装
- 常駐/自動整理(launchd)は未実装
