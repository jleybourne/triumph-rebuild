//! ExecuteIndirect under the program library's root, on WARP: the arguments `rebuild_xlist` rewrites a title
//! command signature into (`console::indirect::library_arguments`) make a valid host command signature against
//! the library's real graphics and compute root signatures. Several `CONSTANT` arguments into one root-constant
//! parameter, a root view's address as two constants, and dwords no stage reads (into spare dwords) are all
//! things the runtime has to accept. WARP only, never the card.
#![cfg(windows)]

use rebuild_pipelines::d3d::{serialize, Gpu};
use rebuild_pipelines::layout::{param, RootDesc, StageLayout, USER_DATA_DWORDS};
use rebuild_xlist::console::indirect::{library_arguments, Unmappable};
use rebuild_xlist::console::{StageUserData, UserMap, UserSource};
use windows::Win32::Graphics::Direct3D12::*;

fn constant(slot: u32, dest: u32, n: u32) -> D3D12_INDIRECT_ARGUMENT_DESC {
    D3D12_INDIRECT_ARGUMENT_DESC {
        Type: D3D12_INDIRECT_ARGUMENT_TYPE_CONSTANT,
        Anonymous: D3D12_INDIRECT_ARGUMENT_DESC_0 {
            Constant: D3D12_INDIRECT_ARGUMENT_DESC_0_1 { RootParameterIndex: slot, DestOffsetIn32BitValues: dest, Num32BitValuesToSet: n },
        },
    }
}

fn view(t: D3D12_INDIRECT_ARGUMENT_TYPE, slot: u32) -> D3D12_INDIRECT_ARGUMENT_DESC {
    D3D12_INDIRECT_ARGUMENT_DESC {
        Type: t,
        Anonymous: D3D12_INDIRECT_ARGUMENT_DESC_0 { ConstantBufferView: D3D12_INDIRECT_ARGUMENT_DESC_0_0 { RootParameterIndex: slot } },
    }
}

fn work(t: D3D12_INDIRECT_ARGUMENT_TYPE) -> D3D12_INDIRECT_ARGUMENT_DESC {
    D3D12_INDIRECT_ARGUMENT_DESC { Type: t, ..Default::default() }
}

fn signature(gpu: &Gpu, root: &ID3D12RootSignature, stride: u32, args: &[D3D12_INDIRECT_ARGUMENT_DESC]) -> windows::core::Result<()> {
    let d = D3D12_COMMAND_SIGNATURE_DESC { ByteStride: stride, NumArgumentDescs: args.len() as u32, pArgumentDescs: args.as_ptr(), NodeMask: 0 };
    let mut s: Option<ID3D12CommandSignature> = None;
    unsafe { gpu.device.CreateCommandSignature(&d, root, &mut s) }
}

#[test]
fn library_signatures_are_valid_host_command_signatures() {
    let gpu = match Gpu::warp() {
        Ok(g) => g,
        Err(e) => {
            println!("skipped: {e}");
            return;
        }
    };
    let stage = StageLayout { user_data: true, shadow: true, ..Default::default() };
    let gfx_root = gpu.root_signature(&serialize(&RootDesc::graphics(&stage, &stage, USER_DATA_DWORDS)).unwrap()).unwrap();
    let cs_root = gpu.root_signature(&serialize(&RootDesc::compute(&stage, USER_DATA_DWORDS)).unwrap()).unwrap();

    use UserSource::*;
    // VS: constant 1 of slot 3, the CBV at slot 2 (lo, hi), constant 2 of slot 3; PS: constant 0 of slot 3
    let mut vs = StageUserData { param: param::VS_USER_DATA, dwords: 5, ..Default::default() };
    vs.src[..5].copy_from_slice(&[Constant { slot: 3, dword: 1 }, Constant { slot: 3, dword: 2 }, AddressLo { slot: 2 }, AddressHi { slot: 2 }, Zero]);
    let mut ps = StageUserData { param: param::PS_USER_DATA, dwords: 2, ..Default::default() };
    ps.src[..2].copy_from_slice(&[Constant { slot: 3, dword: 0 }, Zero]);
    let gfx = UserMap::new(&[vs, ps], param::GFX_SHADOW);

    // the title's: constants 1, 2 of slot 3, its CBV at slot 2, constant 0 of slot 3, 2 constants of slot 9
    // (unread), a draw
    let title = [
        constant(3, 1, 2),
        view(D3D12_INDIRECT_ARGUMENT_TYPE_CONSTANT_BUFFER_VIEW, 2),
        constant(3, 0, 1),
        constant(9, 0, 2),
        work(D3D12_INDIRECT_ARGUMENT_TYPE_DRAW_INDEXED),
    ];
    let stride = 8 + 8 + 4 + 8 + 20;
    let l = library_arguments(&title, &gfx).expect("maps");
    assert_eq!(l.stages, 0b11);
    // VS[0..2], VS[2..4], PS[0], PS spare [2..4], draw
    assert_eq!(l.arguments.len(), 5);
    signature(&gpu, &gfx_root, stride, &l.arguments).expect("the runtime takes the graphics library layout");

    // the host's rule the rewrite keeps: constants rise in (parameter, dword) order, never overlapping
    signature(&gpu, &gfx_root, 32, &[constant(0, 0, 1), constant(0, 1, 1), constant(1, 0, 1), work(D3D12_INDIRECT_ARGUMENT_TYPE_DRAW_INDEXED)]).expect("rising");
    for bad in [[constant(1, 0, 1), constant(0, 0, 1)], [constant(0, 2, 1), constant(0, 0, 1)], [constant(0, 0, 2), constant(0, 1, 1)]] {
        let args = [bad[0], bad[1], work(D3D12_INDIRECT_ARGUMENT_TYPE_DRAW_INDEXED)];
        assert!(signature(&gpu, &gfx_root, 32, &args).is_err(), "the host takes constants out of order now: the rewrite can do more");
    }
    // and a signature that changes no root argument is made with no root signature
    assert!(signature(&gpu, &gfx_root, 20, &[work(D3D12_INDIRECT_ARGUMENT_TYPE_DRAW_INDEXED)]).is_err());
    // the order the title's bytes force: the pixel stage's constant before the vertex stage's
    assert_eq!(library_arguments(&[constant(3, 0, 2), work(D3D12_INDIRECT_ARGUMENT_TYPE_DRAW_INDEXED)], &gfx).err(), Some(Unmappable::Order));

    // compute: constant 0 of slot 1 at dword 5, a root SRV's address at 6, 7
    let mut cs = StageUserData { param: param::CS_USER_DATA, dwords: 8, ..Default::default() };
    cs.src[5] = Constant { slot: 1, dword: 0 };
    cs.src[6] = AddressLo { slot: 4 };
    cs.src[7] = AddressHi { slot: 4 };
    let cmap = UserMap::new(&[cs], param::CS_SHADOW);
    let title = [constant(1, 0, 1), view(D3D12_INDIRECT_ARGUMENT_TYPE_SHADER_RESOURCE_VIEW, 4), work(D3D12_INDIRECT_ARGUMENT_TYPE_DISPATCH)];
    let l = library_arguments(&title, &cmap).expect("maps");
    assert_eq!(l.arguments.len(), 3, "one run: dwords 5, 6, 7 are two arguments (constant, then the address)");
    signature(&gpu, &cs_root, 4 + 8 + 12, &l.arguments).expect("the runtime takes the compute library layout");

    // what the rewrite refuses, the host would have to be given another way
    let title = [view(D3D12_INDIRECT_ARGUMENT_TYPE_VERTEX_BUFFER_VIEW, 0), work(D3D12_INDIRECT_ARGUMENT_TYPE_DRAW)];
    assert_eq!(library_arguments(&title, &gfx).err(), Some(Unmappable::BufferView));
}
