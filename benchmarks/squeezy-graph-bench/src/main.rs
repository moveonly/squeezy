mod accuracy;
mod cli;
mod corpus;
mod execution;
mod gates;
mod harness;
mod mixed;
mod oracles;
mod report;
mod runner;
mod summary;
mod util;

use squeezy_core::Result;

fn main() -> Result<()> {
    runner::main()
}
