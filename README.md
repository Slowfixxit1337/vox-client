# Vox Client

Стендовый клиент проверяет TLS, согласует версию по WSS и входит в комнату.
Готовность медиа отображается отдельно. Публичный протокол загружается
Cargo по закреплённому Git SHA. Для сигнального прогона достаточно Rust
и MSVC. Для окна GPUI добавьте feature `desktop`.

```powershell
cargo test --locked --features lab
cargo build --locked --features lab --bins
.\target\debug\vox-lab.exe init --out .\lab-private
.\target\debug\vox-client.exe --dev --server vox-lab.test:8443 --lab .\lab-private\client-a.toml --media-route sfu
```

На втором клиенте используйте `client-b.toml`. Передайте серверу только
`admission.toml`. Клиент проверяет имя или IP сертификата по системному
хранилищу доверия. Одноразовый секрет допуска хранится в локальном файле.

Проба нативных зависимостей на Windows x64:

```powershell
cargo check --locked --features lab,desktop,webrtc
cargo test --locked --features webrtc --test native_webrtc
```

Выбранные исходники: GPUI Kit и gpui-component 0.11.0, gpui-pre 0.3.14
(zed `f126950f9d3b62666f97437ae5d4ccac2272c4db`).
LiveKit libwebrtc 0.4.2 и webrtc-sys 0.3.55 построены из
`2811b4be194b091868ed83cca0603842179fed36`. Сборочный helper загружает
нативный выпуск `webrtc-a545de4` для Windows x64.


SHA-256 полученной библиотеки webrtc.lib для проверенной Windows-сборки:
3e5959b5320579814706848a0641224db22f03a374493e90cbd235948977f930.
