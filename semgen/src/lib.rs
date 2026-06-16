use builder::Builder;
use context::{Context, Mode};
use liblisa::encoding::{Encoding, IgnoredMetadata};
use log::info;
use sem86_core::arch::intel386::Intel386;
use sem86_core::il::MiniSem;
use sem86_core::system::Db;

use crate::builder::SemSpec;

pub mod builder;
pub mod context;
pub mod dsl;
pub mod instrs;

#[derive(Copy, Clone, Debug, Default, clap::Parser)]
pub struct Config {
    #[clap(long, default_value = "false")]
    pub no_imul_zf: bool,
}

pub fn into_encodings(
    mode: Mode, stack_address_size: Db, builder: impl Builder<Output = SemSpec<Intel386>>,
) -> Vec<Encoding<Intel386, MiniSem<Intel386>, IgnoredMetadata>> {
    let mut result = Vec::new();
    let ctx = Context::new(mode, stack_address_size);
    builder.build(ctx, &mut |ctx, val| {
        if let Some(encoding) = ctx.into_encoding(val) {
            info!("Result: {encoding}",);
            result.push(encoding);
        }
    });

    result
}

#[cfg(test)]
mod tests {
    use sem86_core::system::Db;

    use crate::context::Mode;
    use crate::{Config, into_encodings};

    #[test]
    pub fn test_generation() {
        into_encodings(
            Mode::RealOrProtected16,
            Db::Protected16,
            crate::instrs::flow::builder(Config::default()),
        );
    }
}
