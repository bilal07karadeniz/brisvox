//! The cost of building a request follows the window (what is sent), not the size of the tool
//! results it clips: a deterministic guard by allocation counts and allocated bytes (it
//! replaces the wall-clock ratio of `tests/llm_window.rs`, which could flake on a loaded CI
//! machine).
//!
//! The counting allocator counts only the allocations of the thread that reads the counter, and
//! the builds run on the test thread.
#![allow(clippy::unwrap_used, clippy::indexing_slicing)]
#![allow(unsafe_code)] // the counting allocator forwards to the system allocator

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::sync::Arc;

use brisvox_agent::WindowPolicy;
use brisvox_agent::context::{Context, Message, ToolCall, ToolCallId};
use brisvox_core::TurnEpoch;
use brisvox_engine::{EngineConfig, RequestBuilder};
use serde_json::json;

thread_local! {
    static ALLOCATIONS: Cell<(usize, usize)> = const { Cell::new((0, 0)) };
}

struct Counting;

fn count(bytes: usize) {
    // Const-initialised, destructor-free thread local: reading it never allocates.
    let _ = ALLOCATIONS.try_with(|c| {
        let (n, b) = c.get();
        c.set((n + 1, b + bytes));
    });
}

// SAFETY: forwards every call unchanged to the system allocator; only counts allocations.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        count(layout.size());
        // SAFETY: same contract as the caller's.
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: same contract as the caller's.
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        count(new_size);
        // SAFETY: same contract as the caller's.
        unsafe { System.realloc(ptr, layout, new_size) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        count(layout.size());
        // SAFETY: same contract as the caller's.
        unsafe { System.alloc_zeroed(layout) }
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

fn allocations() -> (usize, usize) {
    ALLOCATIONS.with(Cell::get)
}

/// One user turn with `rounds` tool calls whose results hold `bytes` bytes each.
fn tool_heavy_context(rounds: usize, bytes: usize) -> Context {
    let epoch = TurnEpoch::new(1);
    let mut ctx = Context::with_system("You are a voice agent.");
    ctx.push(Message::user("please look everything up"));
    for round in 0..rounds {
        let id = ToolCallId::new(epoch, u32::try_from(round).unwrap());
        ctx.push(Message::assistant_with_calls(
            "",
            vec![ToolCall {
                id: id.clone(),
                provider_id: None,
                name: Arc::from("lookup"),
                arguments: json!({"n": round}),
                provider_data: None,
            }],
        ));
        ctx.push(Message::tool_result(id, "x".repeat(bytes)));
    }
    ctx
}

/// Allocations and allocated bytes of one build of `context` (after a warm-up build).
fn per_build(builder: &RequestBuilder, context: &Context) -> (usize, usize) {
    builder.build(context, &[]).unwrap();
    let before = allocations();
    let built = builder.build(context, &[]).unwrap();
    let after = allocations();
    drop(built);
    (after.0 - before.0, after.1 - before.1)
}

#[test]
fn a_tool_heavy_build_allocates_the_same_whatever_the_size_of_the_results() {
    let config = EngineConfig::DEFAULT;
    let builder = RequestBuilder::new(
        WindowPolicy::new(1_000, 4),
        config.llm.summary_timeout,
        config.llm.tool_result_floor_tokens,
    );
    let small = per_build(&builder, &tool_heavy_context(12, 20_000));
    let large = per_build(&builder, &tool_heavy_context(12, 60_000));
    println!(
        "per build: 12 x 20 KB {} allocations / {} B, 12 x 60 KB {} / {} B",
        small.0, small.1, large.0, large.1
    );
    // The window holds the clipped results (the same for both sizes): the same allocations,
    // and bytes that do not grow with the results. A build that copied a result before clipping
    // it would allocate 12 x 40 KB more for the large one.
    assert_eq!(small.0, large.0, "allocation counts differ");
    assert!(
        large.1 <= small.1 + 4 * 1024,
        "the large results cost {} B more per build",
        large.1 - small.1
    );
}
