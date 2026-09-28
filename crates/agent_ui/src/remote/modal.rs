//! The Praxis Remote modal: signing in, the paired phones, and turning it
//! off.

use gpui::{
    ClickEvent, ClipboardItem, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable,
    Subscription,
};
use ui::{CommonAnimationExt, Modal, ModalFooter, ModalHeader, Section, prelude::*};
use workspace::{ModalView, Workspace};

use super::store::PhoneInfo;
use super::{PraxisRemote, RemoteStatus};

const ANDROID_APP_URL: &str = "https://github.com/DushyantChetiwal/praxis/releases";
const SIGN_IN_TEXT: &str = "Sign in with your GitHub account to follow and steer the agent \
                            from the Praxis Remote app on your Android phone. There is nothing \
                            else to set up.";
const PRIVACY_TEXT: &str = "Praxis keeps a secret gist in your account as the channel to your \
                            phones. Everything in it is end-to-end encrypted, so GitHub only \
                            sees encrypted data, this computer's name and when it was last \
                            online. A phone can only connect once you allow it here.";
const PERMISSION_TEXT: &str = "Praxis Remote only asks for access to your gists.";

pub(crate) struct PraxisRemoteModal {
    remote: Entity<PraxisRemote>,
    focus_handle: FocusHandle,
    _observe: Subscription,
}

impl PraxisRemoteModal {
    pub(crate) fn toggle(
        workspace: &mut Workspace,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
        let Some(remote) = PraxisRemote::global(cx) else {
            return;
        };
        workspace.toggle_modal(window, cx, |_, cx| Self::new(remote, cx));
    }

    fn new(remote: Entity<PraxisRemote>, cx: &mut Context<Self>) -> Self {
        let observe = cx.observe(&remote, |_, _, cx| cx.notify());
        Self {
            remote,
            focus_handle: cx.focus_handle(),
            _observe: observe,
        }
    }

    fn cancel(&mut self, _: &menu::Cancel, _: &mut Window, cx: &mut Context<Self>) {
        cx.emit(DismissEvent);
    }

    fn signed_out(&self, cx: &mut Context<Self>) -> (AnyElement, ModalFooter) {
        let body = v_flex()
            .gap_2()
            .child(Label::new(SIGN_IN_TEXT))
            .child(muted(PRIVACY_TEXT))
            .into_any_element();
        let on_click = cx.listener(|this, _: &ClickEvent, _, cx| {
            this.remote.update(cx, |remote, cx| remote.sign_in(cx));
        });
        let sign_in = Button::new("praxis-remote-sign-in", "Sign in with GitHub")
            .style(ButtonStyle::Filled)
            .start_icon(Icon::new(IconName::Github).size(IconSize::Small))
            .on_click(on_click);
        (body, ModalFooter::new().end_slot(sign_in))
    }

    fn signing_in(
        &self,
        user_code: String,
        verification_uri: String,
        cx: &mut Context<Self>,
    ) -> (AnyElement, ModalFooter) {
        let instructions = format!("Enter this code at {verification_uri} to sign in:");
        let body = v_flex()
            .gap_2()
            .child(Label::new(instructions))
            .child(Headline::new(user_code.clone()).size(HeadlineSize::XLarge))
            .child(muted(PERMISSION_TEXT))
            .into_any_element();
        let on_cancel = cx.listener(|this, _: &ClickEvent, _, cx| {
            this.remote
                .update(cx, |remote, cx| remote.cancel_sign_in(cx));
        });
        let cancel = Button::new("praxis-remote-cancel", "Cancel").on_click(on_cancel);
        let copy = Button::new("praxis-remote-copy-code", "Copy Code and Open GitHub")
            .style(ButtonStyle::Filled)
            .on_click(move |_, _, cx| {
                cx.write_to_clipboard(ClipboardItem::new_string(user_code.clone()));
                cx.open_url(&verification_uri);
            });
        let buttons = h_flex().gap_1().child(cancel).child(copy);
        (body, ModalFooter::new().end_slot(buttons))
    }

    fn connecting(&self, set_up: bool, cx: &mut Context<Self>) -> (AnyElement, ModalFooter) {
        let text = if set_up {
            "Connecting to GitHub…"
        } else {
            "Contacting GitHub…"
        };
        let body = h_flex()
            .gap_2()
            .child(
                Icon::new(IconName::LoadCircle)
                    .size(IconSize::Small)
                    .color(Color::Muted)
                    .with_rotate_animation(3),
            )
            .child(muted(text))
            .into_any_element();
        let footer = if set_up {
            ModalFooter::new().end_slot(self.turn_off_button(cx))
        } else {
            let on_cancel = cx.listener(|this, _: &ClickEvent, _, cx| {
                this.remote
                    .update(cx, |remote, cx| remote.cancel_sign_in(cx));
            });
            let cancel = Button::new("praxis-remote-cancel", "Cancel").on_click(on_cancel);
            ModalFooter::new().end_slot(cancel)
        };
        (body, footer)
    }

    fn connected(
        &self,
        login: Option<String>,
        device: String,
        phones: Vec<PhoneInfo>,
        offline: Option<String>,
        cx: &mut Context<Self>,
    ) -> (AnyElement, ModalFooter) {
        let login = login.unwrap_or_default();
        let signed_in = format!("Signed in as @{login} on {device}.");
        let mut body = v_flex().gap_3().child(Label::new(signed_in));
        if let Some(reason) = offline {
            let text = format!("GitHub can't be reached; Praxis keeps trying. {reason}");
            body = body.child(problem(IconName::Warning, Color::Warning, text));
        }
        if phones.is_empty() {
            let hint = format!(
                "No phones are paired yet. Install Praxis Remote on your Android phone, sign in \
                 with the same GitHub account and tap {device}. Praxis then asks you here to \
                 allow it, after you compare the code both screens show."
            );
            let get_app = Button::new("praxis-remote-get-app", "Get the Android App")
                .end_icon(Icon::new(IconName::ArrowUpRight).size(IconSize::Small))
                .on_click(|_, _, cx| cx.open_url(ANDROID_APP_URL));
            body = body.child(muted(hint)).child(h_flex().child(get_app));
        } else {
            let mut list = v_flex().gap_1().child(muted("Paired phones"));
            for (index, phone) in phones.iter().enumerate() {
                list = list.child(self.phone_row(index, phone, cx));
            }
            body = body.child(list);
        }
        let footer = ModalFooter::new().end_slot(self.turn_off_button(cx));
        (body.into_any_element(), footer)
    }

    fn failed(
        &self,
        reason: String,
        sign_in: bool,
        set_up: bool,
        cx: &mut Context<Self>,
    ) -> (AnyElement, ModalFooter) {
        let body = problem(IconName::XCircle, Color::Error, reason);
        let label = if sign_in { "Sign In Again" } else { "Retry" };
        let on_retry = cx.listener(move |this, _: &ClickEvent, _, cx| {
            this.remote.update(cx, |remote, cx| {
                if sign_in {
                    remote.sign_in(cx);
                } else {
                    remote.retry(cx);
                }
            });
        });
        let retry = Button::new("praxis-remote-retry", label)
            .style(ButtonStyle::Filled)
            .on_click(on_retry);
        let mut buttons = h_flex().gap_1();
        if set_up {
            buttons = buttons.child(self.turn_off_button(cx));
        }
        (body, ModalFooter::new().end_slot(buttons.child(retry)))
    }

    fn phone_row(&self, index: usize, phone: &PhoneInfo, cx: &mut Context<Self>) -> AnyElement {
        let paired_at = phone.paired_at.with_timezone(&chrono::Local);
        let paired = format!("Paired {}", paired_at.format("%b %-d, %Y"));
        let phone_id = phone.id.clone();
        let on_unpair = cx.listener(move |this, _: &ClickEvent, _, cx| {
            let phone_id = phone_id.clone();
            this.remote
                .update(cx, |remote, cx| remote.unpair(phone_id, cx));
        });
        let unpair = Button::new(("praxis-remote-unpair", index), "Unpair").on_click(on_unpair);
        let name = v_flex()
            .child(Label::new(phone.name.clone()))
            .child(muted(paired));
        h_flex()
            .w_full()
            .justify_between()
            .gap_2()
            .child(name)
            .child(unpair)
            .into_any_element()
    }

    fn turn_off_button(&self, cx: &mut Context<Self>) -> Button {
        let on_click = cx.listener(|this, _: &ClickEvent, _, cx| {
            this.remote.update(cx, |remote, cx| remote.turn_off(cx));
        });
        Button::new("praxis-remote-turn-off", "Turn Off").on_click(on_click)
    }
}

fn muted(text: impl Into<SharedString>) -> Label {
    Label::new(text).size(LabelSize::Small).color(Color::Muted)
}

fn problem(icon: IconName, color: Color, text: String) -> AnyElement {
    let label = Label::new(text).size(LabelSize::Small).color(color);
    h_flex()
        .gap_2()
        .items_start()
        .child(Icon::new(icon).size(IconSize::Small).color(color))
        .child(div().flex_1().child(label))
        .into_any_element()
}

impl EventEmitter<DismissEvent> for PraxisRemoteModal {}

impl Focusable for PraxisRemoteModal {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl ModalView for PraxisRemoteModal {}

impl Render for PraxisRemoteModal {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let remote = self.remote.read(cx);
        let status = remote.status.clone();
        let login = remote.login.clone();
        let device = remote
            .device
            .clone()
            .unwrap_or_else(|| "this computer".into());
        let phones = remote.phones.clone();
        let set_up = login.is_some();
        let (body, footer) = match status {
            RemoteStatus::Off => self.signed_out(cx),
            RemoteStatus::SigningIn {
                user_code,
                verification_uri,
            } => self.signing_in(user_code, verification_uri, cx),
            RemoteStatus::Connecting => self.connecting(set_up, cx),
            RemoteStatus::Connected => self.connected(login, device, phones, None, cx),
            RemoteStatus::Offline(reason) => {
                self.connected(login, device, phones, Some(reason), cx)
            }
            RemoteStatus::Failed { reason, sign_in } => self.failed(reason, sign_in, set_up, cx),
        };
        v_flex()
            .id("praxis-remote-modal")
            .key_context("PraxisRemoteModal")
            .w(rems(34.))
            .elevation_3(cx)
            .overflow_hidden()
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::cancel))
            .child(
                Modal::new("praxis-remote", None)
                    .header(
                        ModalHeader::new()
                            .headline("Praxis Remote")
                            .description("Follow and steer the agent from your Android phone.")
                            .show_dismiss_button(true),
                    )
                    .section(Section::new().child(body))
                    .footer(footer),
            )
    }
}
