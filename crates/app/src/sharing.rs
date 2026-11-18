//! Sharing a Drive item: Copiar link, which first asks before opening a private item to anyone
//! with the link, and Compartilhar, where people are added, changed and removed and link access
//! turns on and off. Drive calls run in the background; their answers land in `update` and
//! `link_answer`.

use openexplorer_gdrive::share::valid_email;
use openexplorer_gdrive::{Access, Drive, Role, Sharing};
use rune::{El, Tx, col, el, row, text};
use rune_kit::{Button, ButtonVariant, Checkbox, Dialog as Modal, Kit, SegmentedControl, Switch};

use crate::actions::{Dialog, selected};
use crate::model::{Entry, Location, Space};
use crate::{Msg, State, drive_task};

/// The open Compartilhar dialog.
pub struct Share {
    /// The account the item is in.
    pub email: String,
    pub item: Entry,
    /// `None` while the first answer is on its way.
    pub sharing: Option<Result<Sharing, String>>,
    /// A change on its way: controls wait for it.
    pub busy: bool,
    /// Why the last change failed, in Google's words.
    pub error: Option<String>,
    /// The role a person added gets.
    pub role: Role,
    /// Whether Google e-mails a person added.
    pub notify: bool,
}

impl Share {
    fn loaded(&self) -> Option<&Sharing> {
        self.sharing.as_ref().and_then(|r| r.as_ref().ok())
    }

    /// Whether changes can be made now: loaded, this account may share, nothing on its way.
    pub fn can_change(&self) -> bool {
        !self.busy && self.loaded().is_some_and(|s| s.can_share)
    }
}

/// Copiar link on an item only people with access can open.
pub struct MakePublic {
    pub email: String,
    pub item: Entry,
    pub link: Option<String>,
}

/// What a background call for Compartilhar found.
pub enum Update {
    /// The first listing.
    Loaded(Result<Sharing, String>),
    /// A change went through (the access as it is now) or failed (Google's reason). `added`:
    /// a person was added, so the e-mail field empties.
    Saved { result: Result<Sharing, String>, added: bool },
}

/// What Copiar link found: the item's access, read before (`opened` false) or after opening it
/// to anyone with the link.
pub struct LinkAnswer {
    /// The account the item is in.
    pub email: String,
    pub item: Entry,
    pub opened: bool,
    pub result: Result<Sharing, String>,
}

/// The account and item sharing acts on: one item selected in a Drive folder.
fn target(s: &mut State, p: usize) -> Option<(String, Entry)> {
    let items = selected(s, p);
    let loc = &s.panes[p].loc;
    let ([item], Location::Drive { email, space, path }) = (items.as_slice(), loc) else {
        if items.len() > 1 {
            s.status = "Selecione um item só para compartilhar.".into();
        }
        return None;
    };
    // Any item can be shared, wherever it's listed (Com estrela, Recentes...), except the
    // shared drives themselves and what's in Lixeira.
    match space {
        Space::SharedDrives if path.is_empty() => {
            s.status = "Os drives compartilhados são compartilhados no site do Google Drive.".into();
            None
        }
        Space::Trash => {
            s.status = "Restaure o item da Lixeira antes de compartilhar.".into();
            None
        }
        _ => Some((email.clone(), item.clone())),
    }
}


// ---------------------------------------------------------------------------------------------
// Copiar link

/// Copiar link: reads the item's access first, to know whether anyone with the link can open it.
pub fn copy_link(s: &mut State, p: usize, _: &mut Tx) {
    let Some((email, item)) = target(s, p) else { return };
    let (id, account) = (item.id.clone(), email.clone());
    s.status = format!("Buscando o link de \"{}\"…", item.name);
    drive_task(s, &email, "compartilhar", move |d| d.sharing(&id), move |result| Msg::Link(LinkAnswer { email: account, item, opened: false, result }));
}

/// An open item's link goes to the clipboard; a private one first asks (when this account may
/// open it), else its link is copied as it is.
pub fn link_answer(s: &mut State, a: LinkAnswer, tx: &mut Tx) {
    let sharing = match a.result {
        Ok(sh) => sh,
        Err(e) => {
            crate::fail(s, e);
            return;
        }
    };
    let Some(link) = sharing.link.clone() else {
        s.status = format!("O Google não informou o link de \"{}\".", a.item.name);
        return;
    };
    if sharing.anyone().is_some() {
        tx.copy(link);
        s.status = format!("Link copiado: qualquer pessoa com o link pode abrir \"{}\".", a.item.name);
    } else if a.opened || !sharing.can_share {
        tx.copy(link);
        s.status = "Link copiado. Só quem já tem acesso consegue abrir (esta conta não pode mudar o compartilhamento).".into();
    } else {
        s.dialog = Some(Dialog::MakePublic(Box::new(MakePublic { email: a.email, item: a.item, link: Some(link) })));
    }
}

/// Permitir e copiar: opens the item to anyone with the link, then copies it.
pub fn make_public(s: &mut State) {
    let Some(Dialog::MakePublic(m)) = s.dialog.take() else { return };
    let MakePublic { email, item, .. } = *m;
    let (id, account) = (item.id.clone(), email.clone());
    s.status = format!("Liberando o link de \"{}\"…", item.name);
    drive_task(s, &email, "compartilhar", move |d| d.set_link_access(&id, true).and_then(|_| d.sharing(&id)), move |result| Msg::Link(LinkAnswer { email: account, item, opened: true, result }));
}

/// Copiar sem mudar o acesso.
pub fn copy_as_is(s: &mut State, tx: &mut Tx) {
    let Some(Dialog::MakePublic(m)) = s.dialog.take() else { return };
    if let Some(link) = m.link {
        tx.copy(link);
        s.status = "Link copiado. Só quem já tem acesso consegue abrir.".into();
    }
}

// ---------------------------------------------------------------------------------------------
// Compartilhar

pub fn ask_share(s: &mut State, p: usize, _: &mut Tx) {
    let Some((email, item)) = target(s, p) else { return };
    s.name_edit = rune::TextEdit::new("");
    let id = item.id.clone();
    let key = item.id.clone();
    drive_task(s, &email, "compartilhar", move |d| d.sharing(&id), move |r| Msg::Share { item: key, update: Update::Loaded(r) });
    s.dialog = Some(Dialog::Share(Box::new(Share { email, item, sharing: None, busy: false, error: None, role: Role::Reader, notify: true })));
}

fn share_mut(s: &mut State) -> Option<&mut Share> {
    match &mut s.dialog {
        Some(Dialog::Share(sh)) => Some(sh),
        _ => None,
    }
}

/// An answer for the Compartilhar of item `item`; dropped if that dialog closed since.
pub fn update(s: &mut State, item: &str, update: Update) {
    let Some(sh) = share_mut(s).filter(|sh| sh.item.id == item) else { return };
    match update {
        Update::Loaded(r) => sh.sharing = Some(r),
        Update::Saved { result, added } => {
            sh.busy = false;
            match result {
                Ok(now) => {
                    sh.sharing = Some(Ok(now));
                    sh.error = None;
                    if added {
                        s.name_edit = rune::TextEdit::new("");
                    }
                }
                Err(e) => sh.error = Some(e),
            }
        }
    }
}

/// Runs a change for the open Compartilhar, then reads the access again.
fn change(s: &mut State, added: bool, work: impl FnOnce(&Drive, &str) -> Result<(), String> + Send + 'static) {
    let Some(sh) = share_mut(s).filter(|sh| sh.can_change()) else { return };
    sh.busy = true;
    sh.error = None;
    let (email, id) = (sh.email.clone(), sh.item.id.clone());
    let key = id.clone();
    drive_task(s, &email, "compartilhar", move |d| work(d, &id).and_then(|_| d.sharing(&id)), move |result| Msg::Share { item: key, update: Update::Saved { result, added } });
}

/// Compartilhar: gives the e-mail typed the role picked.
pub fn add_person(s: &mut State) {
    let typed = s.name_edit.text.trim().to_string();
    let Some(sh) = share_mut(s) else { return };
    if !valid_email(&typed) {
        sh.error = Some(if typed.is_empty() { "Digite o e-mail de uma pessoa ou de um grupo.".into() } else { format!("E-mail inválido: {typed}") });
        return;
    }
    let (role, notify) = (sh.role, sh.notify);
    change(s, true, move |d, id| d.share_with(id, &typed, role, notify));
}

pub fn set_role(s: &mut State, permission: String, role: Role) {
    change(s, false, move |d, id| d.set_role(id, &permission, role));
}

pub fn remove(s: &mut State, permission: String) {
    change(s, false, move |d, id| d.unshare(id, &permission));
}

pub fn set_link(s: &mut State, on: bool) {
    change(s, false, move |d, id| d.set_link_access(id, on));
}

// ---------------------------------------------------------------------------------------------
// Views

fn close(s: &mut State) {
    s.dialog = None;
}

fn role_index(r: Role) -> Option<usize> {
    Role::GRANTED.iter().position(|g| *g == r)
}

fn roles(k: &Kit, key: String, current: Role, enabled: bool, pick: impl Fn(&mut State, Role) + Clone + 'static) -> El {
    let labels = Role::GRANTED.map(Role::label);
    let mut seg = SegmentedControl::new(&labels, role_index(current)).fit().on_pick(move |s: &mut State, i: usize, _: &mut Tx| pick(s, Role::GRANTED[i]));
    if !enabled {
        for i in 0..labels.len() {
            seg = seg.segment_state(i, rune_kit::State::Disabled);
        }
    }
    seg.build(k).key(key)
}

/// One person, group or domain with access, and what can be done about it.
fn person_row(k: &Kit, sh: &Share, p: &openexplorer_gdrive::Permission) -> El {
    let me = p.email.as_deref().is_some_and(|e| e.eq_ignore_ascii_case(&sh.email));
    let (name, sub) = match (p.access, &p.name, &p.email) {
        (Access::Domain, ..) => (p.who(), "Domínio".to_string()),
        (Access::Group, n, e) => (n.clone().or_else(|| e.clone()).unwrap_or_default(), format!("Grupo · {}", e.as_deref().unwrap_or(""))),
        (_, Some(n), Some(e)) if n != e => (n.clone(), e.clone()),
        _ => (p.who(), String::new()),
    };
    let name = if me { format!("{name} (você)") } else { name };
    let sub = if p.deleted { "Conta excluída".to_string() } else { sub };
    let mut who = col().w(0.0).grow(1.0).min_w(0.0).gap(1.0).child(text(name).font(k.ty.text_small).color(k.c.text_base).truncate().min_w(0.0));
    if !sub.is_empty() {
        who = who.child(text(sub).font(k.ty.text_xsmall).color(k.c.text_muted).truncate().min_w(0.0));
    }
    let fixed = p.role == Role::Owner || p.inherited || me || role_index(p.role).is_none();
    let mut line = row().key(format!("perm-{}", p.id)).w_full().h(40.0).gap(10.0).center_y().child(who);
    if fixed {
        let mut label = p.role.label().to_string();
        if p.inherited {
            label.push_str(" · herdado");
        }
        line = line.child(text(label).font(k.ty.text_small).color(k.c.text_muted));
    } else {
        let enabled = sh.can_change();
        let id = p.id.clone();
        line = line.child(roles(k, format!("role-{}", p.id), p.role, enabled, move |s, r| set_role(s, id.clone(), r)));
        let id = p.id.clone();
        line = line.child(Button::new("Remover").variant(ButtonVariant::GhostMuted).disabled(!enabled).on_press(move |s: &mut State| remove(s, id.clone())).build(k).key(format!("remove-{}", p.id)));
    }
    line
}

pub fn share_dialog(s: &State, k: &Kit, sh: &Share, layer: &mut Vec<El>) {
    let can = sh.can_change();
    let muted = |t: &str| text(t.to_string()).font(k.ty.text_small).color(k.c.text_muted).wrap().w_full().min_w(0.0);
    let heading = |t: &str| text(t.to_string()).font(k.ty.text_small_medium).color(k.c.text_base);

    // Add a person: e-mail, role, notify.
    let field = rune::text_input(&s.name_edit, k.ty.ui_compact)
        .single_line()
        .key("share-email")
        .label("E-mail")
        .placeholder("E-mail de uma pessoa ou grupo", k.c.text_muted)
        .grow(1.0)
        .shrink()
        .min_w(0.0)
        .color(k.c.text_base)
        .caret(k.c.text_base, k.selection())
        .on_edit(|s: &mut State| &mut s.name_edit)
        .on_submit(|s: &mut State, _: &mut Tx| add_person(s))
        .autofocus();
    let input = row().key("email").w(0.0).grow(1.0).min_w(0.0).h(32.0).px(10.0).center_y().radius(k.radius.sm).fill(k.c.bg_base).border(1.0, k.c.border_base).child(field);
    let add = row()
        .key("add")
        .w_full()
        .gap(8.0)
        .center_y()
        .child(input)
        .child(Button::new("Compartilhar").variant(ButtonVariant::Submit).disabled(!can).on_press(add_person).build(k).key("share-add"));
    let options = row()
        .key("add-options")
        .w_full()
        .gap(16.0)
        .center_y()
        .child(roles(k, "role-new".into(), sh.role, !sh.busy, |s, r| {
            if let Some(sh) = share_mut(s) {
                sh.role = r;
            }
        }))
        .child(
            Checkbox::new(sh.notify)
                .label("Avisar por e-mail")
                .on_toggle(|s: &mut State| {
                    if let Some(sh) = share_mut(s) {
                        sh.notify = !sh.notify;
                    }
                })
                .build(k)
                .key("notify"),
        );
    let mut body = col().key("content").w_full().px(16.0).pb(16.0).gap(10.0).child(add).child(options);

    match &sh.sharing {
        None => body = body.child(muted("Carregando quem tem acesso…")),
        Some(Err(e)) => body = body.child(text(e.clone()).font(k.ty.text_small).color(k.c.danger.fg).wrap().w_full().min_w(0.0)),
        Some(Ok(sharing)) => {
            if !sharing.can_share {
                body = body.child(muted("Esta conta não pode mudar o compartilhamento deste item."));
            }
            body = body.child(el().w_full().h(1.0).fill(k.c.border_weak)).child(heading("Pessoas com acesso"));
            let mut people = col().key("people").w_full();
            for p in sharing.permissions.iter().filter(|p| p.access != Access::Anyone) {
                people = people.child(person_row(k, sh, p));
            }
            body = body.child(people.max_h(220.0).scroll(rune::ScrollSpec { stick_bottom: false }));

            // Anyone with the link.
            let anyone = sharing.anyone();
            let open = anyone.is_some();
            let note = match anyone {
                Some(p) => format!("Pode abrir sem fazer login, como {}.", p.role.label().to_lowercase()),
                None => "Desativado: só as pessoas acima podem abrir.".into(),
            };
            let switch = Switch::new(open).label("Qualquer pessoa com o link").hide_label().disabled(!can).on_toggle(move |s: &mut State| set_link(s, !open)).build(k).key("link-switch");
            let words = col()
                .w(0.0)
                .grow(1.0)
                .min_w(0.0)
                .gap(1.0)
                .child(text("Qualquer pessoa com o link").font(k.ty.text_small).color(k.c.text_base))
                .child(text(note).font(k.ty.text_xsmall).color(k.c.text_muted).truncate().min_w(0.0));
            let mut link_row = row().key("link").w_full().gap(10.0).center_y().child(switch).child(words);
            if let Some(url) = sharing.link.clone() {
                link_row = link_row.child(
                    Button::new("Copiar link")
                        .variant(ButtonVariant::Neutral)
                        .on_press_tx(move |s: &mut State, tx: &mut Tx| {
                            tx.copy(url.clone());
                            s.status = "Link copiado.".into();
                        })
                        .build(k)
                        .key("copy-link"),
                );
            }
            body = body.child(el().w_full().h(1.0).fill(k.c.border_weak)).child(heading("Acesso pelo link")).child(link_row);
        }
    }
    if let Some(e) = &sh.error {
        body = body.child(text(e.clone()).font(k.ty.text_small).color(k.c.danger.fg).wrap().w_full().min_w(0.0).key("share-error"));
    }
    let status = if sh.busy { "Salvando…" } else { "" };
    body = body.child(
        row()
            .key("actions")
            .w_full()
            .pt(8.0)
            .gap(8.0)
            .center_y()
            .child(text(status).font(k.ty.text_small).color(k.c.text_muted).grow(1.0))
            .child(Button::new("Concluído").variant(ButtonVariant::Submit).on_press(close).build(k)),
    );
    Modal::new("share", format!("Compartilhar \"{}\"", sh.item.name)).open(true).child(body).width(600.0).fit().on_close(close).build(k, layer);
}

pub fn public_dialog(k: &Kit, m: &MakePublic, layer: &mut Vec<El>) {
    let note = format!(
        "Só quem já tem acesso consegue abrir \"{}\" pelo link. Para copiar um link que qualquer pessoa possa abrir, o item passa a ser visível para qualquer pessoa com o link (somente leitura). Dá para desfazer em Compartilhar…",
        m.item.name
    );
    let body = col()
        .key("content")
        .w_full()
        .px(16.0)
        .pb(16.0)
        .gap(12.0)
        .child(text(note).font(k.ty.text_small).color(k.c.text_muted).wrap().w_full().min_w(0.0))
        .child(
            row()
                .key("actions")
                .w_full()
                .pt(8.0)
                .gap(8.0)
                .justify_end()
                .child(Button::new("Cancelar").variant(ButtonVariant::Neutral).on_press(close).build(k))
                .child(Button::new("Copiar sem mudar").variant(ButtonVariant::Neutral).on_press_tx(|s: &mut State, tx: &mut Tx| copy_as_is(s, tx)).build(k))
                .child(Button::new("Permitir e copiar").variant(ButtonVariant::Submit).on_press(make_public).build(k)),
        );
    Modal::new("make-public", "Liberar o link?").open(true).child(body).width(480.0).fit().on_close(close).build(k, layer);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Space;

    fn sharing(anyone: bool, can_share: bool) -> Sharing {
        let mut perms = vec![serde_json::json!({"id": "1", "type": "user", "role": "owner", "emailAddress": "voce@gmail.com"})];
        if anyone {
            perms.push(serde_json::json!({"id": "anyoneWithLink", "type": "anyone", "role": "reader"}));
        }
        let file = serde_json::json!({"webViewLink": "https://drive.google.com/file/d/f1/view", "capabilities": {"canShare": can_share}});
        Sharing::parse(&file, &serde_json::json!({ "permissions": perms })).unwrap()
    }

    fn state() -> State {
        let mut s = crate::init();
        s.panes[1].go(Location::drive("voce@gmail.com", Space::MyDrive));
        s
    }

    fn item() -> Entry {
        Entry { id: "f1".into(), name: "nota.txt".into(), is_dir: false, size: Some(2), modified: None, target: None, mime: None, md5: None, starred: false }
    }

    #[test]
    fn copy_link_asks_before_opening_a_private_item() {
        let mut s = state();
        let mut tx = Tx::default();
        link_answer(&mut s, LinkAnswer { email: "voce@gmail.com".into(), item: item(), opened: false, result: Ok(sharing(false, true)) }, &mut tx);
        assert!(matches!(&s.dialog, Some(Dialog::MakePublic(m)) if m.item.id == "f1" && m.email == "voce@gmail.com"));

        // Already open: copied at once, no question.
        let mut s = state();
        link_answer(&mut s, LinkAnswer { email: "voce@gmail.com".into(), item: item(), opened: false, result: Ok(sharing(true, true)) }, &mut tx);
        assert!(s.dialog.is_none());
        assert!(s.status.starts_with("Link copiado"), "{}", s.status);

        // An account that can't share copies the link as it is.
        let mut s = state();
        link_answer(&mut s, LinkAnswer { email: "voce@gmail.com".into(), item: item(), opened: false, result: Ok(sharing(false, false)) }, &mut tx);
        assert!(s.dialog.is_none() && s.status.contains("Só quem já tem acesso"), "{}", s.status);
    }

    #[test]
    fn a_bad_email_is_refused_before_any_call() {
        let mut s = state();
        s.dialog = Some(Dialog::Share(Box::new(Share { email: "voce@gmail.com".into(), item: item(), sharing: Some(Ok(sharing(false, true))), busy: false, error: None, role: Role::Reader, notify: true })));
        s.name_edit = rune::TextEdit::new("ana@");
        add_person(&mut s);
        let Some(Dialog::Share(sh)) = &s.dialog else { panic!() };
        assert!(!sh.busy && sh.error.as_deref().is_some_and(|e| e.contains("inválido")), "{:?}", sh.error);
    }

    #[test]
    fn a_failed_change_keeps_the_list_and_shows_googles_reason() {
        let mut s = state();
        let before = sharing(false, true);
        s.dialog = Some(Dialog::Share(Box::new(Share { email: "voce@gmail.com".into(), item: item(), sharing: Some(Ok(before.clone())), busy: true, error: None, role: Role::Reader, notify: true })));
        s.name_edit = rune::TextEdit::new("ana@exemplo.com");
        update(&mut s, "f1", Update::Saved { result: Err("Compartilhar: HTTP 403: Sharing is restricted".into()), added: true });
        let Some(Dialog::Share(sh)) = &s.dialog else { panic!() };
        assert!(!sh.busy && sh.error.as_deref() == Some("Compartilhar: HTTP 403: Sharing is restricted"));
        assert_eq!(sh.loaded(), Some(&before));
        assert_eq!(s.name_edit.text, "ana@exemplo.com", "the e-mail stays to fix");

        // An answer for another item is dropped.
        update(&mut s, "other", Update::Loaded(Err("x".into())));
        let Some(Dialog::Share(sh)) = &s.dialog else { panic!() };
        assert_eq!(sh.loaded(), Some(&before));

        update(&mut s, "f1", Update::Saved { result: Ok(sharing(true, true)), added: true });
        let Some(Dialog::Share(sh)) = &s.dialog else { panic!() };
        assert!(sh.error.is_none() && sh.loaded().unwrap().anyone().is_some());
        assert_eq!(s.name_edit.text, "");
    }
}

#[cfg(test)]
mod quota_tests {
    use crate::ui::quota_label;
    use openexplorer_gdrive::Quota;

    #[test]
    fn the_status_bar_says_whats_free_of_the_plan() {
        let q = |limit, usage| Quota { limit, usage, in_drive: 0, in_trash: 0 };
        assert_eq!(quota_label(&q(Some(16_106_127_360), 2_898_000_000)), "12,3 GB livres de 15 GB");
        assert_eq!(quota_label(&q(Some(5_497_558_138_880), 1_507_019_203_121)), "3,6 TB livres de 5 TB");
        assert_eq!(quota_label(&q(Some(1_649_267_441_664), 0)), "1,5 TB livres de 1,5 TB");
        assert_eq!(quota_label(&q(None, 734_003_200)), "700,0 MB usados · Ilimitado");
    }
}
