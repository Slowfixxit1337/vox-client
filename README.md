# Vox Client

Стендовый клиент проверяет TLS, согласует версию по WSS и входит в комнату.
Настройка `lab,webrtc` подключает клиентский ICE/DTLS к SFU через билет
из управляющего API. Экран `desktop` показывает состояние соединения.
Подключение транспорта само по себе не подтверждает голос или E2EE.

```powershell
cargo test --locked --features lab
cargo build --locked --features lab,webrtc --bins
.\target\debug\vox-lab.exe init --out .\lab-private --sfu-url wss://vox-lab.test:8443/v1/sfu
.\target\debug\vox-client.exe --dev --server vox-lab.test:8443 --lab .\lab-private\client-a.toml --media-route sfu
```

На втором клиенте используйте `client-b.toml`. Передайте на стенд только
`admission.toml` и `lab-sfu.key`. Секреты отдельных клиентов держите у их
владельцев. Для лабораторного CA можно указать `test_ca = "test-ca.pem"`
в каждом профиле с путём относительно TOML. Без этого поля клиент берёт
корни из системного хранилища. Проверка имени или IP в SAN обязательна
в обоих режимах. Билет отправляется исключительно по WSS того же origin.

Проба нативных зависимостей на Windows x64:

```powershell
cargo check --locked --features lab,desktop,webrtc
cargo test --locked --features webrtc --test native_webrtc
```

GPUI Kit и gpui-component 0.11.0, gpui-pre 0.3.14
(zed `f126950f9d3b62666f97437ae5d4ccac2272c4db`).
LiveKit libwebrtc 0.4.2 и webrtc-sys 0.3.55 построены из
`2811b4be194b091868ed83cca0603842179fed36`. Сборочный helper загружает
нативный выпуск `webrtc-a545de4` для Windows x64. SHA-256 проверенной
`webrtc.lib` — `3e5959b5320579814706848a0641224db22f03a374493e90cbd235948977f930`.
