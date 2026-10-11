//! macOS：SCDynamicStore 里的代理设置。键名即 SCSchemaDefinitions.h 里 `kSCPropNetProxies*` 常量的值（读这些常量
//! 本身要 unsafe）。

use system_configuration::core_foundation::array::CFArray;
use system_configuration::core_foundation::base::{CFType, ItemRef, TCFType};
use system_configuration::core_foundation::dictionary::CFDictionary;
use system_configuration::core_foundation::number::CFNumber;
use system_configuration::core_foundation::string::CFString;
use system_configuration::dynamic_store::SCDynamicStoreBuilder;

use crate::{Error, MacOs, Server};

/// 读代理设置。SCDynamicStoreCopyProxies 在没有代理设置与出错时都返回 NULL，分不开，都当作没有设置（None）。
pub(crate) fn read() -> Result<Option<MacOs>, Error> {
    let store = SCDynamicStoreBuilder::new("hs-m3u8")
        .build()
        .ok_or_else(|| Error("建不出读取系统设置的 SCDynamicStore 会话".to_owned()))?;
    let Some(proxies) = store.get_proxies() else {
        return Ok(None);
    };
    let proxies = Proxies(&proxies);
    Ok(Some(MacOs {
        http: proxies.server("HTTP")?,
        https: proxies.server("HTTPS")?,
        socks: proxies.server("SOCKS")?,
        exceptions: proxies.strings("ExceptionsList")?,
        exclude_simple_hostnames: proxies.flag("ExcludeSimpleHostnames")?,
        auto_config: proxies.flag("ProxyAutoConfigEnable")?,
        auto_discovery: proxies.flag("ProxyAutoDiscoveryEnable")?,
    }))
}

struct Proxies<'a>(&'a CFDictionary<CFString, CFType>);

impl Proxies<'_> {
    fn value(&self, key: &str) -> Option<ItemRef<'_, CFType>> {
        self.0.find(CFString::new(key))
    }

    /// `<名>Enable` 开着时的 `<名>Proxy` 与 `<名>Port`。
    fn server(&self, name: &str) -> Result<Option<Server>, Error> {
        if !self.flag(&format!("{name}Enable"))? {
            return Ok(None);
        }
        Ok(Some(Server {
            host: self.string(&format!("{name}Proxy"))?,
            port: self.number(&format!("{name}Port"))?,
        }))
    }

    /// 没有这一项时为 false。
    fn flag(&self, key: &str) -> Result<bool, Error> {
        Ok(self.number(key)?.is_some_and(|value| value != 0))
    }

    fn number(&self, key: &str) -> Result<Option<i32>, Error> {
        let Some(value) = self.value(key) else {
            return Ok(None);
        };
        value
            .downcast::<CFNumber>()
            .and_then(|number| number.to_i32())
            .map(Some)
            .ok_or_else(|| wrong_type(key, "整数"))
    }

    fn string(&self, key: &str) -> Result<Option<String>, Error> {
        let Some(value) = self.value(key) else {
            return Ok(None);
        };
        value
            .downcast::<CFString>()
            .map(|string| Some(string.to_string()))
            .ok_or_else(|| wrong_type(key, "字符串"))
    }

    /// 没有这一项时为空。
    fn strings(&self, key: &str) -> Result<Vec<String>, Error> {
        let Some(value) = self.value(key) else {
            return Ok(Vec::new());
        };
        let array = value
            .downcast::<CFArray>()
            .ok_or_else(|| wrong_type(key, "数组"))?;
        array
            .iter()
            .map(|item| {
                // 安全性：CFArray 的元素都是 CF 对象的指针；按 get 规则包装，增加一次引用计数，不取走数组持有的那份
                let item = unsafe { CFType::wrap_under_get_rule(*item) };
                item.downcast::<CFString>()
                    .map(|string| string.to_string())
                    .ok_or_else(|| wrong_type(key, "字符串数组"))
            })
            .collect()
    }
}

fn wrong_type(key: &str, expected: &str) -> Error {
    Error(format!("系统代理设置的 {key} 不是{expected}"))
}
