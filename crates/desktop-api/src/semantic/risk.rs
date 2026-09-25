//! Shared action classification, not a second model gate. Navigation is not a
//! transaction; only recognizable critical commits ask for one confirmation.
use super::{NativeAction, RiskClass};

pub(crate) fn classify_desktop_risk(action: &NativeAction, name: Option<&str>) -> RiskClass {
    if matches!(action, NativeAction::SetValue | NativeAction::SetRangeValue) {
        return RiskClass::BoundedWrite;
    }
    if *action != NativeAction::Invoke {
        return RiskClass::Reversible;
    }
    let name = name.unwrap_or_default().trim().to_lowercase();
    let contains = |words: &[&str]| words.iter().any(|word| name.contains(word));
    if [
        "settings",
        "preferences",
        "history",
        "help",
        "设置",
        "历史",
        "记录",
        "说明",
        "帮助",
    ]
    .iter()
    .any(|suffix| name.ends_with(suffix))
        && !contains(&[
            "delete",
            "erase",
            "wipe",
            "reset",
            "restore",
            "recover",
            "remove",
            "close account",
            "format",
            "confirm",
            "grant",
            "allow",
            "删除",
            "清除",
            "重置",
            "恢复",
            "注销",
            "永久",
            "格式化",
            "确认",
            "授予",
            "允许",
        ])
    {
        return RiskClass::Reversible;
    }
    if contains(&[
        "permanently delete",
        "永久删除",
        "delete account",
        "remove account",
        "close account",
        "注销账户",
        "注销账号",
        "删除账户",
        "删除账号",
        "delete all data",
        "erase all data",
        "wipe all data",
        "清除所有数据",
        "删除所有数据",
        "factory reset",
        "restore factory settings",
        "恢复出厂设置",
        "format drive",
        "format disk",
        "格式化磁盘",
        "格式化驱动器",
        "purchase",
        "place order",
        "confirm order",
        "支付",
        "付款",
        "提交订单",
        "buy now",
        "transfer funds",
        "wire transfer",
        "confirm transfer",
        "转账",
        "grant permission",
        "allow access",
        "授予权限",
        "允许访问",
    ]) {
        RiskClass::DestructiveCritical
    } else if contains(&["send", "发送", "publish", "发布", "submit", "提交"]) {
        RiskClass::ExternalCommit
    } else if contains(&["save", "保存", "apply", "应用"]) {
        RiskClass::BoundedWrite
    } else {
        RiskClass::Reversible
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn navigation_and_commit_are_distinct_on_every_platform() {
        for name in [
            "支付设置",
            "付款历史",
            "Permission settings",
            "Security settings",
            "转账记录",
        ] {
            assert_eq!(
                classify_desktop_risk(&NativeAction::Invoke, Some(name)),
                RiskClass::Reversible,
                "{name}"
            );
        }
        for name in [
            "恢复出厂设置",
            "Restore factory settings",
            "永久删除所有记录",
            "Confirm purchase",
            "确认付款",
        ] {
            assert_eq!(
                classify_desktop_risk(&NativeAction::Invoke, Some(name)),
                RiskClass::DestructiveCritical,
                "{name}"
            );
        }
        assert_eq!(
            classify_desktop_risk(&NativeAction::SetValue, Some("Confirm purchase")),
            RiskClass::BoundedWrite
        );
    }
}
