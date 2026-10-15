// performance.
// Fragment of the bootstrap function body: all files share one scope and
// are concatenated in file-name order by build.rs.

// ------------------------------------------------------------ performance
const timeOrigin = ops.timeOrigin;
const performance = {
  now: ops.now,
  timeOrigin,
  mark() {},
  measure() {},
  toJSON() { return { timeOrigin }; },
};
define(g, "performance", performance);
