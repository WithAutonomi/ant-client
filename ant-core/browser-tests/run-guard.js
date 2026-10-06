// Fails an otherwise passing run unless every discovered test ran in Chromium and passed.
// Playwright alone exits 0 when tests are skipped, so a skipped test could leave the suite green
// without exercising the pinned nodes.
export default class RunGuard {
  onBegin(_, suite) { this.tests = suite.allTests(); }
  onEnd(result) {
    // `--list` reports the discovered tests without running them, so there is nothing to check.
    if (result.status !== "passed" || process.argv.includes("--list")) return;
    const tests = this.tests ?? [];
    const missed = tests.filter(test => test.results.at(-1)?.status !== "passed");
    if (tests.length > 0 && missed.length === 0) {
      console.log(`Run guard: all ${tests.length} tests ran and passed`);
      return;
    }
    console.error(tests.length === 0 ? "Run guard: no tests ran"
      : `Run guard: ${missed.length} of ${tests.length} tests did not pass: ${missed.map(test => test.title).join("; ")}`);
    return { status: "failed" };
  }
}
