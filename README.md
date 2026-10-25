# claude-remote

Windows の PC で動いている Claude デスクトップアプリのウィンドウだけを、別の PC のブラウザから見て操作するためのツール。全体を見せるリモートデスクトップではなく、Claude app のウィンドウ1つだけを配信する。

- **映像**：Windows.Graphics.Capture でウィンドウを取り込み、OpenH264 で H.264 にエンコードして、WebSocket で送る。ブラウザは WebCodecs でデコードする
- **入力**：同じ WebSocket で、マウス・ホイール・キー・文字列（IME で確定したもの）を送り、`SendInput` で注入する
- **外からの接続**：Cloudflare Tunnel を通す。認証は Cloudflare Access に任せる

同じ PC で計測したキャプチャから表示までの遅延は、中央値 68 ms / p95 84 ms（1920x1152、約 22 fps、ソフトウェアエンコード）。

## 安全について

Claude app は PC 上のファイルやブラウザを操作できるので、このツールを乗っ取られると PC を乗っ取られるのに近い。

- ホストには独自の認証がない。**必ず 127.0.0.1 で待ち受け、Cloudflare Access の後ろに置くこと**。`--bind` で外に開くと誰でも操作できてしまう
- cloudflared の設定で `originRequest.access.required: true` を指定し、cloudflared 自身にも Access のトークンを検証させる（[tunnel/config.example.yml](tunnel/config.example.yml)）
- クリックとホイールは、その位置にあるウィンドウが Claude app 本体か、本体が持つダイアログのときだけ注入する
- キーは Claude app が前面にあるときだけ送る。Alt と Win は送らず、Ctrl は Ctrl+Enter と編集系（A/C/V/X/Z/Y）だけ
- ログには入力した文字列の中身を残さず、文字数だけ記録する

## 構成

| パス | 内容 |
| --- | --- |
| `src/bin/claude-remote-host.rs` | ホスト本体。キャプチャ、エンコード、HTTP/WebSocket サーバー |
| `src/main.rs` | 検証用のプローブ。ウィンドウごとの PNG と合成画像、report.json を出す |
| `src/capture.rs` | Windows.Graphics.Capture によるキャプチャ |
| `src/encoder.rs` | OpenH264 によるエンコード |
| `src/input.rs` | 入力の検査と注入 |
| `src/ws.rs` | WebSocket での配信 |
| `src/windows_enum.rs` | Claude app のウィンドウ探し |
| `web/index.html` | 受信ページ（ホストに埋め込まれる） |
| `tunnel/config.example.yml` | cloudflared の設定の雛形 |
| `deploy/` | ログオン時に自動で起動するタスクの登録スクリプト |

## 使い方

### ビルド

Rust（stable、MSVC）でビルドする。OpenH264 はソースからビルドされる。

```powershell
cargo build --release
```

フォルダのパスが長いと、ビルドスクリプトのリンクが Windows のパス長制限（LNK1104）で失敗する。その場合は `.cargo/config.toml` に短い `target-dir` を指定する。

### 手元で動かす

```powershell
target\release\claude-remote-host.exe
```

`http://localhost:8080/` を開く。WebCodecs は HTTPS か localhost でしか使えない。同じ PC から操作すると、受信ページのブラウザが Claude app の上に重なるので、クリックは安全チェックで拒否される。動作確認は別の PC から Tunnel 経由で行う。

### Cloudflare Tunnel と Access

1. `cloudflared tunnel login`
2. `cloudflared tunnel create claude-remote`
3. `cloudflared tunnel route dns claude-remote <ホスト名>`
4. Zero Trust で、そのホスト名の Access アプリ（self_hosted）を作る。許可するのは自分のメールだけにする
5. [tunnel/config.example.yml](tunnel/config.example.yml) を `%USERPROFILE%\.cloudflared\claude-remote.yml` にコピーし、Tunnel ID、ホスト名、チーム名、Access アプリの AUD を入れる

### ログオン時に自動で起動する

```powershell
powershell -ExecutionPolicy Bypass -File deploy\install-tasks.ps1 -HostExe target\release\claude-remote-host.exe
```

`%USERPROFILE%\claude-remote` にホストをコピーし、タスク スケジューラにログオン時のタスクを2つ登録する。止まっても自動で再起動し、ログは `logs\` に出る。ホストは Windows サービスにはできない（サービスはセッション 0 で動き、デスクトップをキャプチャできないため）。

## 制約

- PC はログオンしたまま、ロックせずに動かす必要がある。ロック画面と UAC の画面はキャプチャも操作もできない
- 1920x1152 のままソフトウェアでエンコードしているので、30 fps には届かない。表示サイズへの縮小や HW エンコーダーは未対応
- 受信できるのは同時に1台だけ。新しい接続が来ると、前の接続はコード 4000 で閉じられる
