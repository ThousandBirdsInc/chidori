// Near-empty script used to measure each runtime's process-startup baseline
// (binary load + realm/global setup), which the harness subtracts from the
// workload totals to estimate execution-only time.
console.log("RESULT=ok");
