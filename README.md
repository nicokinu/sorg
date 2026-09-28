# sorg-app — スクショ整理ダッシュボード (Tauri)

macOS のデスクトップのスクリーンショットをサムネイル一覧で確認しながら、
AI判定をもとにフォルダ整理できる Tauri 2 アプリ。

## 仕組み

```
スクショ走査 (walkdir)
  → Apple Vision OCR (apple-vision crate、ローカル・瞬時)
  → Ollaya laya:multilingual に choice(カテゴリ) + score(保存価値) を質問 (~10ms/枚)
  → ~/.sorg/cache.db にハッシュ単位でキャッシュ(2回目以降は即表示)
  → UIで選択 → ~/Pictures/<category>/ へ移動
```

- 判定はすべてローカル完結(Ollaya が `localhost:11435` で起動している必要あり)
- 整理先は `~/Pictures/<category>`(設定 `archive_root` で変更可)。
  Desktop には `整理済み` というシンボリックリンクを置いてあると便利
- 「ゴミ箱」は `~/.Trash` への移動で、完全削除ではない
- CLI版(`~/Projects/screenshot-organizer`の`sorg`)と `~/.sorg`(config.json / cache.db)を共有

## 使い方

```bash
cd ~/Projects/sorg-app
npm install          # 初回のみ
npx tauri dev        # 開発実行
npx tauri build      # .app / .dmg バンドル
```

1. アプリを開くと Desktop のスクショが一覧表示される(未判定は `pending`)
2. **「未分類を判定」** → OCR+Ollaya判定。進捗バーが動く
3. チップバーでカテゴリ別フィルタ / 「tech を全选択」/「全選択」/「選択解除」
4. **「選択を整理」** → `~/Pictures/<category>/` へ移動(確認ダイアログあり)
5. **「＋ カテゴリ追加」** → 名前と判定基準を入力して追加。カテゴリチップの「× foo」で
   削除すると、判定基準から外れ、そのカテゴリに移動済みのファイルは Desktop に戻す
6. カードの **⌘クリック** で同一カテゴリを一括選択

## 制限・これから

- 画像のみのスクショ(動画サムネ等)は OCR 対象外 → `no-text` 表示のまま
- `no-text` の分類には vision モデルが必要
- 常駐モード・launchd 自動起動は未実装
