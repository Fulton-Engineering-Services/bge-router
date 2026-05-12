// Copyright (c) 2026 J. Patrick Fulton
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! Thin entry point for the bge-router.
//!
//! All orchestration logic lives in [`bge_router::run`]; this binary only
//! initialises tracing and hands off.

use bge_router::run;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    bge_router::logging::init();
    run().await
}
