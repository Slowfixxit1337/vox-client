use crate::auth_net::Client;
use gpui_kit::base::{OtpEvent, OtpInput as BaseOtpInput, OtpState};
use gpui_kit::component::{
    Disableable, Theme,
    button::{Button, ButtonVariants},
    input::{Input, InputContentType, InputEvent, InputState},
};
use gpui_kit::{
    AnyElement, Context, Entity, Focusable, FontWeight, SharedString, Subscription,
    TitlebarOptions, Window, WindowBounds, WindowOptions, div, prelude::*, px, rgb, size, svg,
};
use std::{
    borrow::Cow,
    time::{Duration, Instant},
};
use tokio::runtime::Handle;
use vox_protocol::auth::{AuthError, Operation, Outcome};

const BACKGROUND: u32 = 0x020617;
const FIELD: u32 = 0x0f172a;
const BORDER: u32 = 0x1e293b;
const TEXT: u32 = 0xe2e8f0;
const MUTED: u32 = 0xcbd5e1;
const AMBER: u32 = 0xf59e0b;
const PRIMARY: u32 = 0xd97706;
const ERROR: u32 = 0xfca5a5;
const LOGO: &[u8] = include_bytes!("../assets/auth/auth-logo.svg");
const REGISTRATION_LOGO: &[u8] = include_bytes!("../assets/auth/registration-logo.svg");
const CONFIRMATION_LOGO: &[u8] = include_bytes!("../assets/auth/confirmation-logo.svg");
const PASSWORD_ICON: &[u8] = include_bytes!("../assets/auth/password.svg");
const SIGN_IN_ICON: &[u8] = include_bytes!("../assets/auth/signin-login.svg");
const REGISTRATION_ICON: &[u8] = include_bytes!("../assets/auth/registration-login.svg");
const MAIL_ICON: &[u8] = include_bytes!("../assets/auth/email.svg");

pub fn show(client: Client) {
    let handle = Handle::current();
    gpui_kit::application().run(move |cx| {
        gpui_kit::init(cx);
        let _ = cx
            .text_system()
            .add_fonts(vec![Cow::Borrowed(include_bytes!(
                "../assets/auth/Inter.ttf"
            ))]);
        Theme::update(cx, |theme| {
            theme.font_family = "Inter".into();
            theme.background = rgb(BACKGROUND).into();
            theme.foreground = rgb(TEXT).into();
            theme.muted_foreground = rgb(MUTED).into();
            theme.input = rgb(BORDER).into();
            theme.ring = rgb(AMBER).into();
            theme.caret = rgb(AMBER).into();
            theme.button_primary = rgb(PRIMARY).into();
            theme.button_primary_hover = rgb(AMBER).into();
            theme.button_primary_active = rgb(PRIMARY).into();
            theme.shadow = false;
        });
        cx.on_window_closed(|cx, _| {
            if cx.windows().is_empty() {
                cx.quit();
            }
        })
        .detach();
        let options = WindowOptions {
            window_bounds: Some(WindowBounds::centered(None, size(px(480.), px(540.)), cx)),
            titlebar: Some(TitlebarOptions {
                title: Some("Vox".into()),
                ..Default::default()
            }),
            is_resizable: false,
            ..Default::default()
        };
        if let Err(error) = gpui_kit::open_window(options, cx, |window, cx| {
            cx.new(|cx| AuthView::new(client, handle, window, cx))
        }) {
            eprintln!("Could not open the Vox account window: {error}");
            cx.quit();
        }
    });
}

struct Session {
    username: String,
    token: String,
}

struct Verification {
    token: String,
    email: String,
    resend_at: Instant,
    expires_at: Instant,
    confirmed: Option<Session>,
    checking_since: Option<Instant>,
    rejected_at: Option<Instant>,
    retry_code: bool,
}

enum Screen {
    SignIn,
    Registration,
    Confirmation(Verification),
    Friends(Session),
}

#[derive(Clone, Copy)]
enum Request {
    Register,
    SignIn,
    Confirm,
    Resend,
    Resume,
    SignOut,
    CancelConfirmed,
    Remember,
}

struct AuthView {
    client: Client,
    runtime: Handle,
    screen: Screen,
    login: Entity<InputState>,
    username: Entity<InputState>,
    email: Entity<InputState>,
    password: Entity<InputState>,
    otp: Entity<OtpState>,
    busy: bool,
    status: Option<&'static str>,
    _subscriptions: Vec<Subscription>,
}

impl AuthView {
    fn new(client: Client, runtime: Handle, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let login = cx.new(|cx| InputState::new(window, cx).placeholder("Login"));
        let username = cx.new(|cx| InputState::new(window, cx).placeholder("Login"));
        let email = cx.new(|cx| InputState::new(window, cx).placeholder("Mail"));
        let password = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("Password")
                .masked(true)
        });
        let otp = cx.new(|cx| OtpState::new(6, window, cx));
        let subscriptions = vec![
            cx.subscribe_in(&password, window, |view, _, event, window, cx| {
                if matches!(event, InputEvent::PressEnter { .. }) {
                    view.submit(window, cx);
                }
            }),
            cx.subscribe_in(&otp, window, |view, _, event, window, cx| match event {
                OtpEvent::Complete => view.submit(window, cx),
                _ => cx.notify(),
            }),
        ];
        cx.defer_in(window, |view, window, cx| {
            view.focus(window, cx);
            view.resume(window, cx);
        });
        cx.spawn_in(window, async move |view, cx| {
            loop {
                cx.background_executor().timer(Duration::from_secs(1)).await;
                if view
                    .update_in(cx, |view, window, cx| {
                        if let Screen::Confirmation(verification) = &view.screen {
                            if !view.busy
                                && verification.confirmed.is_none()
                                && verification.expires_at <= Instant::now()
                            {
                                if !view.otp.read(cx).value().is_empty() {
                                    view.otp.update(cx, |otp, cx| otp.set_value("", window, cx));
                                }
                                view.status = Some(error_text(AuthError::ExpiredCode));
                            }
                            cx.notify();
                        }
                    })
                    .is_err()
                {
                    break;
                }
            }
        })
        .detach();
        Self {
            client,
            runtime,
            screen: Screen::SignIn,
            login,
            username,
            email,
            password,
            otp,
            busy: false,
            status: None,
            _subscriptions: subscriptions,
        }
    }

    fn focus(&self, window: &mut Window, cx: &mut Context<Self>) {
        match &self.screen {
            Screen::SignIn => self.login.read(cx).focus_handle(cx).focus(window, cx),
            Screen::Registration => self.username.read(cx).focus_handle(cx).focus(window, cx),
            Screen::Confirmation(verification) if verification.confirmed.is_none() => {
                self.otp.update(cx, |otp, cx| otp.focus(window, cx));
            }
            _ => {}
        }
    }

    fn switch(&mut self, screen: Screen, window: &mut Window, cx: &mut Context<Self>) {
        self.screen = screen;
        self.status = None;
        self.password
            .update(cx, |password, cx| password.set_value("", window, cx));
        self.focus(window, cx);
        cx.notify();
    }

    fn resume(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.run(Request::Resume, window, cx, |client| async move {
            let Some(session) = client.saved_session()? else {
                return Ok(None);
            };
            let outcome = client.call(Operation::ResumeSession { session }).await?;
            if matches!(
                outcome,
                Outcome::Rejected {
                    code: AuthError::SessionExpired | AuthError::InvalidCredentials,
                    ..
                }
            ) {
                client.clear_session()?;
            }
            Ok(Some(outcome))
        });
    }

    fn run<F, Fut>(
        &mut self,
        request: Request,
        window: &mut Window,
        cx: &mut Context<Self>,
        action: F,
    ) where
        F: FnOnce(Client) -> Fut + Send + 'static,
        Fut: Future<Output = anyhow::Result<Option<Outcome>>> + Send + 'static,
    {
        if self.busy {
            return;
        }
        self.busy = true;
        self.status = None;
        let client = self.client.clone();
        let task = self.runtime.spawn(async move { action(client).await });
        cx.spawn_in(window, async move |view, cx| {
            let result = task.await;
            let _ = view.update_in(cx, |view, window, cx| {
                view.busy = false;
                if matches!(request, Request::Confirm)
                    && let Screen::Confirmation(verification) = &mut view.screen
                {
                    verification.checking_since = None;
                    verification.retry_code = !matches!(result, Ok(Ok(Some(_))));
                }
                match result {
                    Ok(Ok(Some(outcome))) => view.outcome(request, outcome, window, cx),
                    Ok(Ok(None)) => {}
                    _ => {
                        view.status = Some(match request {
                            Request::Confirm => "Couldn't verify the code.",
                            Request::Remember => {
                                "Vox couldn't save this session on this PC. Try again."
                            }
                            _ => "Vox couldn't connect to the account service. Try again.",
                        })
                    }
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn send(
        &mut self,
        request: Request,
        operation: Operation,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.run(request, window, cx, move |client| async move {
            let outcome = client.call(operation).await?;
            match (&request, &outcome) {
                (Request::SignIn, Outcome::Authenticated { session, .. }) => {
                    client.remember_session(session)?
                }
                (
                    Request::SignOut,
                    Outcome::SignedOut
                    | Outcome::Rejected {
                        code: AuthError::SessionExpired | AuthError::InvalidCredentials,
                        ..
                    },
                ) => client.clear_session()?,
                _ => {}
            }
            Ok(Some(outcome))
        });
    }

    fn submit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        match &self.screen {
            Screen::SignIn | Screen::Registration => {
                let password = self.password.read(cx).value().to_string();
                let (request, operation) = if matches!(self.screen, Screen::SignIn) {
                    let login = self.login.read(cx).value().trim().to_owned();
                    if login.is_empty() || password.is_empty() {
                        self.status = Some("Enter your login and password.");
                        cx.notify();
                        return;
                    }
                    (Request::SignIn, Operation::SignIn { login, password })
                } else {
                    let username = self.username.read(cx).value().trim().to_owned();
                    let email = self.email.read(cx).value().trim().to_owned();
                    if username.is_empty() || email.is_empty() || password.is_empty() {
                        self.status = Some("Enter a login, email and password.");
                        cx.notify();
                        return;
                    }
                    (
                        Request::Register,
                        Operation::Register {
                            username,
                            email,
                            password,
                        },
                    )
                };
                self.password
                    .update(cx, |password, cx| password.set_value("", window, cx));
                self.send(request, operation, window, cx);
            }
            Screen::Confirmation(verification) => {
                if let Some(session) = &verification.confirmed {
                    let username = session.username.clone();
                    let session = session.token.clone();
                    self.run(Request::Remember, window, cx, |client| async move {
                        client.remember_session(&session)?;
                        Ok(Some(Outcome::Authenticated {
                            username,
                            session,
                            expires_in_seconds: 0,
                        }))
                    });
                    return;
                }
                let code = self.otp.read(cx).value().to_string();
                if code.len() != 6 || !code.bytes().all(|digit| digit.is_ascii_digit()) {
                    self.status = Some("Enter the six-digit code.");
                    cx.notify();
                    return;
                }
                if Instant::now() >= verification.expires_at {
                    self.status = Some("This code has expired. Request a new code.");
                    cx.notify();
                    return;
                }
                let verification = verification.token.clone();
                if let Screen::Confirmation(verification) = &mut self.screen {
                    verification.checking_since = Some(Instant::now());
                    verification.retry_code = false;
                }
                self.send(
                    Request::Confirm,
                    Operation::ConfirmEmail { verification, code },
                    window,
                    cx,
                );
            }
            Screen::Friends(_) => {}
        }
    }

    fn resend(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Screen::Confirmation(verification) = &self.screen
            && verification.confirmed.is_none()
            && Instant::now() >= verification.resend_at
        {
            let verification = verification.token.clone();
            self.send(
                Request::Resend,
                Operation::ResendCode { verification },
                window,
                cx,
            );
        }
    }

    fn cancel(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Screen::Confirmation(verification) = &self.screen {
            if let Some(session) = &verification.confirmed {
                let session = session.token.clone();
                self.send(
                    Request::CancelConfirmed,
                    Operation::SignOut { session },
                    window,
                    cx,
                );
            } else {
                self.switch(Screen::Registration, window, cx);
            }
        }
    }

    fn sign_out(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Screen::Friends(session) = &self.screen {
            let session = session.token.clone();
            self.send(Request::SignOut, Operation::SignOut { session }, window, cx);
        }
    }

    fn outcome(
        &mut self,
        request: Request,
        outcome: Outcome,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match (request, outcome) {
            (
                Request::Register | Request::SignIn | Request::Resend,
                Outcome::VerificationRequired {
                    verification,
                    email,
                    retry_after_seconds,
                    expires_in_seconds,
                },
            ) => {
                let now = Instant::now();
                self.otp.update(cx, |otp, cx| otp.set_value("", window, cx));
                self.switch(
                    Screen::Confirmation(Verification {
                        token: verification,
                        email,
                        resend_at: now + Duration::from_secs(retry_after_seconds.into()),
                        expires_at: now + Duration::from_secs(expires_in_seconds.into()),
                        confirmed: None,
                        checking_since: None,
                        rejected_at: None,
                        retry_code: false,
                    }),
                    window,
                    cx,
                );
            }
            (
                Request::Confirm,
                Outcome::Authenticated {
                    username, session, ..
                },
            ) => {
                if let Screen::Confirmation(verification) = &mut self.screen {
                    verification.confirmed = Some(Session {
                        username,
                        token: session,
                    });
                }
            }
            (
                Request::SignIn | Request::Resume | Request::Remember,
                Outcome::Authenticated {
                    username, session, ..
                },
            ) => {
                self.switch(
                    Screen::Friends(Session {
                        username,
                        token: session,
                    }),
                    window,
                    cx,
                );
            }
            (
                Request::SignOut | Request::CancelConfirmed,
                Outcome::SignedOut
                | Outcome::Rejected {
                    code: AuthError::SessionExpired | AuthError::InvalidCredentials,
                    ..
                },
            ) => {
                self.switch(Screen::SignIn, window, cx);
            }
            (
                _,
                Outcome::Rejected {
                    code,
                    retry_after_seconds,
                },
            ) => {
                if let Screen::Confirmation(verification) = &mut self.screen {
                    let now = Instant::now();
                    if retry_after_seconds > 0 {
                        verification.resend_at = verification
                            .resend_at
                            .max(now + Duration::from_secs(retry_after_seconds.into()));
                    }
                    if code == AuthError::ExpiredCode {
                        verification.expires_at = now;
                    }
                    if matches!(code, AuthError::InvalidCode | AuthError::ExpiredCode) {
                        if code == AuthError::InvalidCode {
                            verification.rejected_at = Some(now);
                        }
                        self.otp.update(cx, |otp, cx| otp.set_value("", window, cx));
                        self.focus(window, cx);
                    }
                }
                self.status = Some(error_text(code));
            }
            _ => self.status = Some("Vox returned an unexpected account response. Try again."),
        }
    }

    fn credentials(&self, registration: bool, cx: &mut Context<Self>) -> AnyElement {
        let login = if registration {
            &self.username
        } else {
            &self.login
        };
        let login_icon = if registration {
            REGISTRATION_ICON
        } else {
            SIGN_IN_ICON
        };
        let body = div()
            .flex()
            .flex_col()
            .child(field(
                login,
                "Login",
                InputContentType::Username,
                login_icon,
                self.busy,
            ))
            .when(registration, |body| {
                body.child(div().mt(px(16.)).child(field(
                    &self.email,
                    "Mail",
                    InputContentType::EmailAddress,
                    MAIL_ICON,
                    self.busy,
                )))
            })
            .child(
                div()
                    .mt(px(if registration { 24. } else { 16. }))
                    .child(field(
                        &self.password,
                        "Password",
                        if registration {
                            InputContentType::NewPassword
                        } else {
                            InputContentType::Password
                        },
                        PASSWORD_ICON,
                        self.busy,
                    )),
            )
            .when(!registration, |body| {
                body.child(
                    div().flex().justify_end().mt(px(12.)).child(
                        link("forgot-password", "Forgot password?", self.busy)
                            .text_color(rgb(MUTED))
                            .on_click(cx.listener(|view, _, _, cx| {
                                view.status = Some("Password recovery is not available yet.");
                                cx.notify();
                            })),
                    ),
                )
            });
        let footer = div()
            .flex()
            .flex_col()
            .gap(px(12.))
            .child(
                primary(
                    "submit-form",
                    if registration { "Register" } else { "Sign in" },
                    self.busy,
                )
                .on_click(cx.listener(|view, _, window, cx| view.submit(window, cx))),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_center()
                    .gap(px(4.))
                    .text_size(px(14.))
                    .child(if registration {
                        "Have account?"
                    } else {
                        "No account?"
                    })
                    .child(
                        link(
                            "switch-form",
                            if registration { "Login" } else { "Register" },
                            self.busy,
                        )
                        .on_click(cx.listener(
                            move |view, _, window, cx| {
                                view.switch(
                                    if registration {
                                        Screen::SignIn
                                    } else {
                                        Screen::Registration
                                    },
                                    window,
                                    cx,
                                );
                            },
                        )),
                    ),
            );
        self.layout(
            if registration {
                "Registration"
            } else {
                "Authentication"
            },
            if registration {
                REGISTRATION_LOGO
            } else {
                LOGO
            },
            body.mt(px(40.)).into_any_element(),
            footer.into_any_element(),
            None,
        )
    }

    fn confirmation(
        &self,
        verification: &Verification,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let confirmed = verification.confirmed.is_some();
        let now = Instant::now();
        let wait = verification.resend_at.saturating_duration_since(now);
        let seconds = wait.as_secs() + u64::from(wait.subsec_nanos() > 0);
        let expired = verification.expires_at <= now;
        let state = self.otp.read(cx);
        let digits = state.value();
        let focused = state.focus_handle(cx).is_focused(window);
        let cursor = digits.chars().count();
        let pulse = verification.checking_since.map(|since| {
            window.request_animation_frame();
            let phase = since.elapsed().as_secs_f32() * std::f32::consts::TAU;
            0.45 + 0.2 * phase.sin()
        });
        let shake = verification.rejected_at.map_or(0., |since| {
            let progress = (since.elapsed().as_secs_f32() / 0.24).min(1.);
            if progress < 1. {
                window.request_animation_frame();
            }
            (progress * std::f32::consts::TAU * 3.).sin() * (1. - progress) * 5.
        });
        let cells = div()
            .relative()
            .left(px(shake))
            .flex()
            .gap(px(12.))
            .children((0..6).map(|position| {
                let digit = digits.chars().nth(position);
                let active = focused && position == cursor;
                let border = if let Some(opacity) = pulse {
                    gpui_kit::Hsla::from(rgb(BORDER))
                        .blend(gpui_kit::Hsla::from(rgb(AMBER)).opacity(opacity))
                } else {
                    rgb(if active { AMBER } else { BORDER }).into()
                };
                div()
                    .size(px(48.))
                    .flex()
                    .justify_center()
                    .items_center()
                    .bg(rgb(FIELD))
                    .border_1()
                    .border_color(border)
                    .rounded(px(12.))
                    .text_size(px(20.))
                    .font_weight(FontWeight::SEMIBOLD)
                    .children(digit.map(|digit| digit.to_string()))
            }));
        let focus_otp = self.otp.clone();
        let body =
            div()
                .mx_auto()
                .w(px(348.))
                .flex()
                .flex_col()
                .gap(px(12.))
                .child(
                    div()
                        .text_size(px(16.))
                        .line_height(px(19.))
                        .text_color(rgb(MUTED))
                        .child("Enter verification code"),
                )
                .child(
                    BaseOtpInput::new(&self.otp)
                        .disabled(self.busy || expired || confirmed)
                        .child(cells.on_mouse_down(
                            gpui_kit::MouseButton::Left,
                            move |_, window, cx| {
                                focus_otp.update(cx, |state, cx| state.focus(window, cx));
                            },
                        )),
                )
                .child(
                    div()
                        .text_size(px(16.))
                        .text_center()
                        .text_color(rgb(MUTED))
                        .when(confirmed, |bar| bar.child("Email confirmed"))
                        .when(!confirmed && seconds > 0, |bar| {
                            bar.child(format!("Resend code in {seconds}s"))
                        })
                        .when(!confirmed && seconds == 0, |bar| {
                            bar.child(link("resend-code", "Resend code", self.busy).on_click(
                                cx.listener(|view, _, window, cx| view.resend(window, cx)),
                            ))
                        })
                        .when(verification.retry_code, |bar| {
                            bar.child(div().mt(px(8.)).child(
                                link("retry-code", "Retry", self.busy).on_click(
                                    cx.listener(|view, _, window, cx| view.submit(window, cx)),
                                ),
                            ))
                        }),
                )
                .into_any_element();
        let footer = div()
            .flex()
            .flex_col()
            .items_center()
            .gap(px(12.))
            .child(
                primary("confirm-email", "Continue", self.busy || !confirmed)
                    .on_click(cx.listener(|view, _, window, cx| view.submit(window, cx))),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(4.))
                    .text_size(px(14.))
                    .child("Wrong account?")
                    .child(
                        link("cancel-confirmation", "Cancel", self.busy)
                            .on_click(cx.listener(|view, _, window, cx| view.cancel(window, cx))),
                    ),
            );
        self.layout(
            "Confirmation",
            CONFIRMATION_LOGO,
            div().mt(px(29.)).child(body).into_any_element(),
            footer.into_any_element(),
            Some(format!("Code sent to {}", mask_email(&verification.email)).into()),
        )
    }

    fn friends(&self, session: &Session, cx: &mut Context<Self>) -> AnyElement {
        let account = format!("@{}", session.username);
        self.layout(
            "Friends",
            LOGO,
            div()
                .mt(px(40.))
                .text_center()
                .child("No friends yet.")
                .into_any_element(),
            primary("sign-out", "Sign out", self.busy)
                .on_click(cx.listener(|view, _, window, cx| view.sign_out(window, cx)))
                .into_any_element(),
            Some(account.into()),
        )
    }

    fn layout(
        &self,
        title: &'static str,
        logo: &'static [u8],
        body: AnyElement,
        footer: AnyElement,
        subtitle: Option<SharedString>,
    ) -> AnyElement {
        div()
            .flex()
            .justify_center()
            .size_full()
            .bg(rgb(BACKGROUND))
            .text_color(rgb(TEXT))
            .font_family("Inter")
            .child(
                div()
                    .w(px(384.))
                    .h_full()
                    .pt(px(64.))
                    .pb(px(35.))
                    .flex()
                    .flex_col()
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .items_center()
                            .child(icon(logo, 50., AMBER))
                            .child(
                                div()
                                    .mt(px(16.))
                                    .text_size(px(24.))
                                    .line_height(px(29.))
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .child(title),
                            )
                            .children(subtitle.map(|text| {
                                div()
                                    .mt(px(8.))
                                    .text_size(px(16.))
                                    .line_height(px(19.))
                                    .text_color(rgb(MUTED))
                                    .child(text)
                            })),
                    )
                    .child(body)
                    .child(
                        div()
                            .flex_1()
                            .flex()
                            .items_center()
                            .justify_center()
                            .text_center()
                            .text_size(px(13.))
                            .text_color(rgb(ERROR))
                            .children(self.status),
                    )
                    .child(footer),
            )
            .into_any_element()
    }
}

impl Render for AuthView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        match &self.screen {
            Screen::SignIn => self.credentials(false, cx),
            Screen::Registration => self.credentials(true, cx),
            Screen::Confirmation(verification) => self.confirmation(verification, window, cx),
            Screen::Friends(session) => self.friends(session, cx),
        }
    }
}

fn icon(bytes: &'static [u8], width: f32, color: u32) -> impl IntoElement {
    svg().data(bytes).size(px(width)).text_color(rgb(color))
}

fn field(
    state: &Entity<InputState>,
    label: &'static str,
    content_type: InputContentType,
    bytes: &'static [u8],
    disabled: bool,
) -> impl IntoElement {
    div()
        .h(px(48.))
        .w_full()
        .bg(rgb(FIELD))
        .border_1()
        .border_color(rgb(BORDER))
        .rounded(px(12.))
        .child(
            Input::new(state)
                .appearance(false)
                .bordered(false)
                .aria_label(label)
                .content_type(content_type)
                .prefix(icon(bytes, 24., MUTED))
                .disabled(disabled)
                .h_full()
                .w_full(),
        )
}

fn primary(id: &'static str, label: &'static str, disabled: bool) -> Button {
    Button::new(id)
        .primary()
        .label(label)
        .disabled(disabled)
        .w_full()
        .h(px(48.))
        .rounded(px(12.))
        .text_color(rgb(TEXT))
        .text_size(px(20.))
        .font_weight(FontWeight::BOLD)
}

fn link(id: &'static str, label: &'static str, disabled: bool) -> Button {
    Button::new(id)
        .text()
        .label(label)
        .disabled(disabled)
        .p_0()
        .h(px(18.))
        .text_color(rgb(AMBER))
        .text_size(px(14.))
}

fn mask_email(email: &str) -> String {
    match email.split_once('@') {
        Some((name, domain)) => format!("{}***@{domain}", name.chars().next().unwrap_or('*')),
        None => "***".into(),
    }
}

fn error_text(code: AuthError) -> &'static str {
    match code {
        AuthError::InvalidInput => "Check the fields and try again.",
        AuthError::AlreadyExists => "That login or email is already in use.",
        AuthError::InvalidCredentials => "The login or password is incorrect.",
        AuthError::InvalidCode => "That code is incorrect. Try the latest code.",
        AuthError::ExpiredCode => "This code has expired. Request a new code.",
        AuthError::RateLimited => "Too many attempts. Wait before trying again.",
        AuthError::SessionExpired => "Your session expired. Sign in again.",
        AuthError::Unavailable => "The account service is unavailable. Try again.",
    }
}
