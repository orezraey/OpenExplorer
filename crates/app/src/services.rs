//! Cloud services reached through WebDAV, offered ready-made in "Registrar conta": each one's
//! server and folder filled in, and what to type where, so a user needs only their login. The
//! addresses are the ones each provider documents for WebDAV (checked on 2026-11-26).
//!
//! Some put the account in the address: Stack (the TransIP account), kDrive (the drive's id) and
//! Onlime (the hosting, which is also the login) are typed as that part alone and the address is
//! made from it; InfiniCLOUD's address is copied whole from the user's page there.

use openexplorer_i18n::t;
use openexplorer_remote::Brand;

/// Where a service's server is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Host {
    /// One server for everyone.
    Fixed(&'static str),
    /// Made from a part the user types (`{}` in the pattern), named by `label`.
    Part { pattern: &'static str, label: (&'static str, &'static str), hint: (&'static str, &'static str) },
    /// Made from the user name.
    FromUser(&'static str),
    /// Copied whole from the provider's page.
    Pasted { hint: &'static str },
}

pub struct Service {
    pub brand: Brand,
    pub name: &'static str,
    /// One line under the name in the catalog.
    pub about: (&'static str, &'static str),
    /// Words the catalog's search finds it by, besides its name.
    pub keywords: &'static str,
    pub host: Host,
    /// The WebDAV folder, empty for the server's root.
    pub root: &'static str,
    pub user: (&'static str, &'static str),
    pub password: (&'static str, &'static str),
    /// Where the password comes from and what else to know, shown under the fields.
    pub note: (&'static str, &'static str),
    pub business: bool,
    /// The server takes the user name in lower case only.
    pub lowercase_user: bool,
}

impl Service {
    pub fn about(&self) -> &'static str {
        t(self.about.0, self.about.1)
    }

    pub fn user_label(&self) -> &'static str {
        t(self.user.0, self.user.1)
    }

    pub fn password_label(&self) -> &'static str {
        t(self.password.0, self.password.1)
    }

    pub fn note(&self) -> &'static str {
        t(self.note.0, self.note.1)
    }

    /// The server's name, from what was typed in the server field and as the user name: the
    /// fixed server, the pattern filled with a bare part, or what was typed when it's already an
    /// address (it has a dot).
    pub fn host_for(&self, typed: &str, user: &str) -> String {
        let typed = typed.trim();
        match self.host {
            Host::Fixed(h) => h.to_string(),
            Host::Part { pattern, .. } if !typed.is_empty() && !typed.contains('.') => pattern.replace("{}", typed),
            Host::FromUser(pattern) if !user.trim().is_empty() => pattern.replace("{}", user.trim()),
            Host::Part { .. } | Host::FromUser(_) | Host::Pasted { .. } => typed.to_string(),
        }
    }
}

const SERVICES: &[Service] = &[
    Service {
        brand: Brand::Koofr,
        name: "Koofr",
        about: ("WebDAV · e-mail e senha de app", "WebDAV · e-mail and app password"),
        keywords: "koofr webdav nuvem cloud europa europe",
        host: Host::Fixed("app.koofr.net"),
        root: "/dav/Koofr",
        user: ("E-mail da conta", "Account e-mail"),
        password: ("Senha de app", "App password"),
        note: (
            "Crie uma senha de app no site do Koofr, em Preferências › Senha › Senhas de app: a senha normal não funciona.",
            "Create an app password on Koofr's website, in Preferences › Password › App passwords: the usual password doesn't work.",
        ),
        business: false,
        lowercase_user: false,
    },
    Service {
        brand: Brand::InfiniCloud,
        name: "InfiniCLOUD",
        about: ("WebDAV · ID de usuário e senha de apps", "WebDAV · user ID and apps password"),
        keywords: "infinicloud infini teracloud webdav nuvem cloud japão japan",
        host: Host::Pasted { hint: "https://xxx.teracloud.jp/dav/" },
        root: "/dav/",
        user: ("ID de usuário", "User ID"),
        password: ("Senha de apps", "Apps password"),
        note: (
            "Na Minha Página do InfiniCLOUD, ative Conexão de apps e copie o endereço WebDAV e a senha de apps que aparecem ali.",
            "On your InfiniCLOUD My Page, turn on Apps Connection and copy the WebDAV address and the apps password shown there.",
        ),
        business: false,
        lowercase_user: false,
    },
    Service {
        brand: Brand::MagentaCloud,
        name: "MagentaCLOUD",
        about: ("Telekom · WebDAV com senha separada", "Telekom · WebDAV with a separate password"),
        keywords: "magentacloud magenta telekom deutsche webdav nuvem cloud alemanha germany",
        host: Host::Fixed("magentacloud.de"),
        root: "/remote.php/webdav",
        user: ("E-mail ou login Telekom", "E-mail or Telekom login"),
        password: ("Senha WebDAV", "WebDAV password"),
        note: (
            "Crie uma senha separada na MagentaCLOUD pelo navegador: avatar › Einstellungen › Geräte und Sitzungen.",
            "Create a separate password in MagentaCLOUD in the browser: avatar › Einstellungen › Geräte und Sitzungen.",
        ),
        business: false,
        lowercase_user: false,
    },
    Service {
        brand: Brand::MailRu,
        name: "Mail.ru Cloud",
        about: ("WebDAV · e-mail e senha de app", "WebDAV · e-mail and app password"),
        keywords: "mail.ru mailru облако webdav nuvem cloud rússia russia",
        host: Host::Fixed("webdav.cloud.mail.ru"),
        root: "",
        user: ("Endereço de e-mail", "E-mail address"),
        password: ("Senha para aplicativo externo", "Password for an external app"),
        note: (
            "A senha normal não funciona: crie uma senha para aplicativo externo em account.mail.ru › Segurança.",
            "The usual password doesn't work: create a password for an external app in account.mail.ru › Security.",
        ),
        business: false,
        lowercase_user: false,
    },
    Service {
        brand: Brand::Yandex,
        name: "Yandex Disk",
        about: ("WebDAV · plano Yandex 360 e senha de app", "WebDAV · Yandex 360 plan and app password"),
        keywords: "yandex disk yandex.disk яндекс диск webdav nuvem cloud",
        host: Host::Fixed("webdav.yandex.ru"),
        root: "",
        user: ("Login Yandex", "Yandex login"),
        password: ("Senha de app (WebDAV)", "App password (WebDAV)"),
        note: (
            "Precisa de um plano Yandex 360. Crie uma senha de app do tipo WebDAV em Yandex ID › Segurança. Pelo WebDAV, apagar não manda para a lixeira.",
            "Needs a Yandex 360 plan. Create an app password of the WebDAV kind in Yandex ID › Security. Over WebDAV, deleting skips the trash.",
        ),
        business: false,
        lowercase_user: false,
    },
    Service {
        brand: Brand::Stack,
        name: "Stack",
        about: ("TransIP · WebDAV", "TransIP · WebDAV"),
        keywords: "stack transip stackstorage webdav nuvem cloud holanda netherlands",
        host: Host::Part { pattern: "{}.stack.storage", label: ("Conta TransIP", "TransIP account"), hint: ("nome da conta (o início do endereço do Stack)", "account name (the start of your Stack address)") },
        root: "/remote.php/webdav",
        user: ("Usuário do Stack (em minúsculas)", "Stack user name (in lowercase)"),
        password: ("Senha (ou token de acesso, com 2FA)", "Password (or access token, with 2FA)"),
        note: (
            "O endereço completo aparece no Stack › nome da conta › configurações › WebDav. Com verificação em duas etapas, use um token de acesso.",
            "The full address is shown in Stack › account name › settings › WebDav. With two-step verification, use an access token.",
        ),
        business: true,
        // TransIP: the user name only works in lowercase.
        lowercase_user: true,
    },
    Service {
        brand: Brand::Onlime,
        name: "Onlime limeDRIVE",
        about: ("Suíça · WebDAV da hospedagem", "Switzerland · the hosting's WebDAV"),
        keywords: "onlime limedrive webdav nuvem cloud suíça suica switzerland hosting",
        host: Host::FromUser("{}-dav.onlime.ch"),
        root: "",
        user: ("Hospedagem (webNNN)", "Hosting (webNNN)"),
        password: ("Senha de FTPS", "FTPS password"),
        note: (
            "Ative o WebDAV da hospedagem antes. O usuário é o nome da hospedagem, como web123.",
            "Turn on the hosting's WebDAV first. The user name is the hosting's name, like web123.",
        ),
        business: true,
        lowercase_user: false,
    },
    Service {
        brand: Brand::HiDrive,
        name: "HiDrive (STRATO)",
        about: ("WebDAV · conforme o pacote", "WebDAV · depending on the plan"),
        keywords: "hidrive strato webdav nuvem cloud alemanha germany",
        host: Host::Fixed("webdav.hidrive.strato.com"),
        root: "",
        user: ("Usuário do HiDrive", "HiDrive user name"),
        password: ("Senha", "Password"),
        note: (
            "O acesso por WebDAV depende do pacote do HiDrive (às vezes é um complemento). Sua pasta pessoal fica em users/<usuário>.",
            "WebDAV access depends on the HiDrive plan (sometimes an add-on). Your own folder is under users/<user name>.",
        ),
        business: true,
        lowercase_user: true,
    },
    Service {
        brand: Brand::HiDriveIonos,
        name: "HiDrive (IONOS)",
        about: ("WebDAV · ative nas configurações", "WebDAV · turn it on in the settings"),
        keywords: "hidrive ionos 1&1 webdav nuvem cloud",
        host: Host::Fixed("webdav.hidrive.ionos.com"),
        root: "",
        user: ("Usuário do HiDrive", "HiDrive user name"),
        password: ("Senha", "Password"),
        note: (
            "Ative o WebDAV antes, no HiDrive: Configurações › Direitos de acesso e protocolos. Sua pasta pessoal fica em users/<usuário>.",
            "Turn on WebDAV first, in HiDrive: Settings › Access rights and protocols. Your own folder is under users/<user name>.",
        ),
        business: true,
        lowercase_user: true,
    },
    Service {
        brand: Brand::KDrive,
        name: "kDrive",
        about: ("Infomaniak · WebDAV nos planos pagos", "Infomaniak · WebDAV on paid plans"),
        keywords: "kdrive infomaniak ksuite webdav nuvem cloud suíça suica switzerland",
        host: Host::Part { pattern: "{}.connect.kdrive.infomaniak.com", label: ("ID do kDrive", "kDrive ID"), hint: ("o número depois de /drive/ no endereço do app", "the number after /drive/ in the app's address") },
        root: "",
        user: ("E-mail da Infomaniak", "Infomaniak e-mail"),
        password: ("Senha de aplicativo", "Application password"),
        note: (
            "Não está disponível nos planos kSuite gratuito e Standard. Crie uma senha de aplicativo no Manager da Infomaniak.",
            "Not available on the free and Standard kSuite plans. Create an application password in Infomaniak's Manager.",
        ),
        business: true,
        lowercase_user: false,
    },
    Service {
        brand: Brand::PCloud,
        name: "pCloud US",
        about: ("WebDAV · contas da região dos EUA", "WebDAV · accounts in the US region"),
        keywords: "pcloud webdav nuvem cloud eua usa us",
        host: Host::Fixed("webdav.pcloud.com"),
        root: "",
        user: ("E-mail", "E-mail"),
        password: ("Senha", "Password"),
        note: (
            "Para contas criadas na região dos EUA (as da Europa usam pCloud EU). Na primeira conexão o pCloud pede, por e-mail, para aprovar o novo dispositivo.",
            "For accounts made in the US region (European ones use pCloud EU). On the first connection pCloud asks, by e-mail, to approve the new device.",
        ),
        business: false,
        lowercase_user: false,
    },
    Service {
        brand: Brand::PCloudEu,
        name: "pCloud EU",
        about: ("WebDAV · contas da região da Europa", "WebDAV · accounts in the European region"),
        keywords: "pcloud webdav nuvem cloud europa europe eu",
        host: Host::Fixed("ewebdav.pcloud.com"),
        root: "",
        user: ("E-mail", "E-mail"),
        password: ("Senha", "Password"),
        note: (
            "Para contas criadas na região da Europa (as dos EUA usam pCloud US). Na primeira conexão o pCloud pede, por e-mail, para aprovar o novo dispositivo.",
            "For accounts made in the European region (US ones use pCloud US). On the first connection pCloud asks, by e-mail, to approve the new device.",
        ),
        business: false,
        lowercase_user: false,
    },
    Service {
        brand: Brand::MailCom,
        name: "Mail.com Cloud",
        about: ("WebDAV · senha específica de aplicativo", "WebDAV · application-specific password"),
        keywords: "mail.com mailcom webdav nuvem cloud",
        host: Host::Fixed("storage-file-us.mail.com"),
        root: "",
        user: ("Endereço mail.com", "mail.com address"),
        password: ("Senha específica de aplicativo", "Application-specific password"),
        note: (
            "Use uma senha específica de aplicativo, criada nas configurações de segurança do mail.com.",
            "Use an application-specific password, made in mail.com's security settings.",
        ),
        business: false,
        lowercase_user: false,
    },
];

/// Every service, in catalog order.
pub fn all() -> &'static [Service] {
    SERVICES
}

/// The service of `brand`; none for Nextcloud and ownCloud, which are software anyone runs.
pub fn of(brand: Brand) -> Option<&'static Service> {
    SERVICES.iter().find(|s| s.brand == brand)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_cloud_brand_is_a_service_once() {
        for b in Brand::ALL {
            let n = SERVICES.iter().filter(|s| s.brand == b).count();
            assert_eq!(n, usize::from(!matches!(b, Brand::Nextcloud | Brand::OwnCloud)), "{b:?}");
        }
    }

    #[test]
    fn addresses_come_from_the_part_typed() {
        let stack = of(Brand::Stack).unwrap();
        assert_eq!(stack.host_for("minhaconta", "ana"), "minhaconta.stack.storage");
        assert_eq!(stack.host_for("https://minhaconta.stack.storage/remote.php/webdav/", "ana"), "https://minhaconta.stack.storage/remote.php/webdav/");
        assert_eq!(of(Brand::KDrive).unwrap().host_for(" 123456 ", "a@b.ch"), "123456.connect.kdrive.infomaniak.com");
        assert_eq!(of(Brand::Onlime).unwrap().host_for("", "web123"), "web123-dav.onlime.ch");
        assert_eq!(of(Brand::Koofr).unwrap().host_for("anything", "a@b.c"), "app.koofr.net");
        assert_eq!(of(Brand::InfiniCloud).unwrap().host_for("https://ana.teracloud.jp/dav/", "ana"), "https://ana.teracloud.jp/dav/");
    }
}
