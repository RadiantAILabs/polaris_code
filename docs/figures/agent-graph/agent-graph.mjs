// agent-graph.mjs — the Polaris Code agent graph as a moving Glyph kit diagram.
//
// Twelve boxes, grouped from the systems `CodeAgent::build` adds
// (crates/polaris_code/src/agents/code_agent.rs). A token plays one short task through them.
// Drawn with the kit's own Grid (boxes, routed lines, packets, border chase), so it matches the
// kit's `states` look; animate-svg.mjs turns the frames into an SVG.
//
// Grouping: Load docs = refresh_project_docs + init_loop; Model = begin_model_call + act +
// accept_response; Retry = decide_retry + wait_to_retry; Fail = abandon + report_failure.
export const title = "The Polaris Code agent graph";
export const intent = "Each box is a step. The token walks one short task: a tool round, a failed model call that is retried, then the answer.";
export const source = "Grouped from CodeAgent::build in crates/polaris_code/src/agents/code_agent.rs";

export default async function build(load) {
  const { Grid } = await load("live/grid.mjs");
  const W = 13, STEP = 24, LEFT = 3;
  const x = (i) => LEFT + i * STEP;
  const mid = (i) => x(i) + Math.floor(W / 2);
  const ROW = [4, 11, 18]; // box top rows; row 0 is the caption, row 2 the top lane
  const node = (name, col, row, role = "") => ({ name, col, row, top: ROW[row], role });
  const N = {
    input: node("Input", 0, 0),
    docs: node("Load docs", 1, 0),
    steer: node("Steer", 2, 0),
    compact: node("Compact", 3, 0),
    summarize: node("Summarize", 4, 0),
    prepare: node("Prepare", 1, 1),
    model: node("Model", 2, 1),
    retry: node("Retry", 3, 1),
    fail: node("Fail", 4, 1, "fail"),
    run: node("Run tools", 0, 2),
    review: node("Review", 1, 2),
    answer: node("Answer", 3, 2, "ok"),
  };
  const r1 = (n) => n.top + 1; // a box's middle row
  // Straight neighbours on one row, either direction.
  const right = (a, b, label) => ({ a, b, label, pts: [[r1(a), x(a.col) + W], [r1(b), x(b.col) - 1]], lab: [r1(a), x(a.col) + W, x(b.col) - 2] });
  const left = (a, b, label) => ({ a, b, label, pts: [[r1(a), x(a.col) - 1], [r1(b), x(b.col) + W]], lab: [r1(a), x(b.col) + W + 1, x(a.col) - 1] });
  // Out of one box's top or bottom, along a lane row, into another box's top or bottom.
  const lane = (a, b, y, ca, cb, label) => {
    const ya = y < a.top ? a.top - 1 : a.top + 3;
    const yb = y < b.top ? b.top - 1 : b.top + 3;
    return { a, b, label, pts: [[ya, ca], [y, ca], [y, cb], [yb, cb]], lab: [y, Math.min(ca, cb), Math.max(ca, cb)] };
  };
  const E = {
    in: right(N.input, N.docs),
    docs: right(N.docs, N.steer),
    steer: right(N.steer, N.compact),
    manual: right(N.compact, N.summarize, "manual"),
    resume: lane(N.summarize, N.steer, 2, mid(4), mid(2) + 2),
    auto: lane(N.compact, N.prepare, 8, mid(3), mid(1)),
    call: right(N.prepare, N.model),
    failed: right(N.model, N.retry, "failed"),
    again: lane(N.retry, N.model, 9, mid(3) + 2, mid(2) + 2, "again"),
    giveUp: right(N.retry, N.fail, "give up"),
    tools: lane(N.model, N.review, 15, mid(2) - 2, mid(1), "tool calls"),
    done: lane(N.model, N.answer, 15, mid(2) + 2, mid(3), "done"),
    approve: left(N.review, N.run, "approved"),
    // Results go back to the top of the loop by the left margin and the top lane.
    results: { a: N.run, b: N.steer, label: "results", pts: [[r1(N.run), x(0) - 1], [r1(N.run), 1], [2, 1], [2, mid(2) - 2], [N.steer.top - 1, mid(2) - 2]], lab: [2, 1, mid(2) - 2] },
  };
  const edges = Object.values(E);
  const path = [
    E.in, E.docs, E.steer, E.auto, E.call, E.tools, E.approve, E.results,
    E.steer, E.auto, E.call, E.failed, E.again, E.done,
  ];
  const rows = ROW[2] + 3, cols = x(4) + W + 1;
  const HOLD = 10, SPEED = 2; // steps resting in a box; cells the token moves per step
  const route = (e) => new Grid(1, 1).route(e.pts);
  const legs = path.map((e) => ({ e, cells: route(e), len: Math.ceil(route(e).length / SPEED) }));
  const period = legs.reduce((s, l) => s + HOLD + l.len, 0) + HOLD * 2;

  const frame = (step) => {
    const s = step % period;
    const g = new Grid(rows, cols);
    // Where the token is: resting in a box, or moving along a leg.
    let t = s, at = path[0].a, moving = null, since = 0;
    for (const l of legs) {
      if (t < HOLD) break;
      t -= HOLD; since += HOLD;
      if (t < l.len) { moving = { l, t }; at = null; break; }
      t -= l.len; since += l.len; at = l.e.b;
    }
    if (!moving && at === path.at(-1).b && t >= HOLD) at = path[0].a; // pause at the end, then restart
    edges.forEach((e) => g.line(e.pts, moving?.l.e === e ? "" : "mute"));
    if (moving) g.packet(moving.l.cells, moving.t * SPEED);
    edges.forEach((e) => {
      if (!e.label) return;
      const [y, c0, c1] = e.lab;
      g.label(y, c0, c1, ` ${e.label} `, moving?.l.e === e ? "" : "mute");
    });
    Object.values(N).forEach((n) => {
      const here = n === at;
      g.box(n.top, x(n.col), W, n.name.toUpperCase(), here ? n.role : "mute");
      if (here) g.chase(n.top, x(n.col), W, s * 2.4, n.role);
    });
    const now = moving ? `${moving.l.e.a.name} → ${moving.l.e.b.name}` : at.name;
    g.scramble(0, 0, now, moving ? moving.t : t, s);
    return g;
  };
  // The still for reduced motion: resting in Model on the first call.
  const still = legs.slice(0, 5).reduce((s, l) => s + HOLD + l.len, 0) + 6;
  const alt = `${title}. Boxes ${Object.values(N).map((n) => n.name).join(", ")}; the token walks ${[path[0].a, ...path.map((e) => e.b)].map((n) => n.name).join(" → ")}.`;
  return { frame, period, still, alt };
}
