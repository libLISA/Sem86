use liblisa::encoding::{EncodingRef, IgnoredMetadata};
use log::trace;

use crate::SegmentSizes;
use crate::arch::intel386::Intel386;
use crate::codegen::backends::{Backend, UninstantiatedBackendFn};
use crate::codegen::lir::MirToLir;
use crate::codegen::mir::MirBuilder;
use crate::codegen::mm::bump::BumpCodeAlloc;
use crate::emulator::DefaultJitBackend;
use crate::il::{MakeEncoding, MiniSemRef};

type _F = Option<<DefaultJitBackend as Backend>::UninstantiatedFn>;
type _M = Option<fn()>;

pub struct SingleEncodingExecution<B: Backend> {
    backend: B,
    functions_ready: Vec<Vec<Option<B::UninstantiatedFn>>>,
    num_compiled: usize,
    bump: BumpCodeAlloc,
}

impl<B: Backend> SingleEncodingExecution<B> {
    pub fn new(backend: B, num_encodings: usize) -> Self {
        Self {
            backend,
            functions_ready: vec![vec![None; num_encodings]; 4],
            num_compiled: 0,
            bump: BumpCodeAlloc::new(32 << 20),
        }
    }

    #[inline(always)]
    pub fn get_or_build<'r>(
        &mut self, index: usize, encoding: EncodingRef<'r, Intel386, MiniSemRef<'r, Intel386>, IgnoredMetadata>,
        protected_mode_memory_accesses: bool, segment_sizes: SegmentSizes, before_building: impl FnOnce(),
    ) -> B::UninstantiatedFn {
        let compilation_key = segment_sizes.is_cs32() as usize * 2 + protected_mode_memory_accesses as usize;
        let functions = &mut self.functions_ready[compilation_key];

        match functions[index] {
            Some(f) => f,
            None => {
                trace!("Compiling encoding: {}", encoding.make_encoding());
                before_building();
                let mir = MirBuilder::build_from_uninstantiated_encoding(
                    encoding,
                    protected_mode_memory_accesses,
                    segment_sizes.is_cs32(),
                );
                let lir = MirToLir::new(&mir).build();

                // TODO: Return last jump condition in metadata.

                let obj = self.backend.codegen_lir_object(&lir).expect("compilation should succeed");
                let (_, f) = self.bump.alloc(&obj).next().unwrap();

                self.num_compiled += 1;

                functions[index] = Some(unsafe { UninstantiatedBackendFn::from_ptr(f) });
                *functions[index].as_ref().unwrap()
            },
        }
    }

    pub fn num_compiled(&self) -> usize {
        self.num_compiled
    }

    pub fn memory_usage(&self) -> usize {
        self.bump.memory_usage()
    }
}
