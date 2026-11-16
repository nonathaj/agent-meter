// agent-meter for the Herdr app.
//
// In the sidebar's footer: the account each provider is on now, and how full
// it is. Clicking it opens every account with each of its limits, and what
// agent-meter can do with it. A switch the watcher makes is announced.
//
// Prototype. Everything comes from `agent-meter list --json`; actions run the
// agent-meter CLI. Nothing here polls a provider.

import { View, div, image, svg } from "gpui-kit";
import { v_flex, h_flex } from "gpui-base";
import { agents, exec, run, theme, props, layout, openOverlay, closeOverlay, notify, watchFolder, checking } from "herdr";

const PROVIDERS = [
  { key: "claude", name: "Claude Code", short: "Claude", app: ["claude-code", "claude"], icon: "icons/claude.svg" },
  { key: "codex", name: "Codex", short: "Codex", app: ["codex"], icon: "icons/codex.svg" },
];
const MONTHS = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
// The footer's own rows: Settings and Add machine are 28 px with 8 px under them.
const ROW = 28;
const FOOTER = 69;

function find(node, id) {
  if (node?.id === id) return node;
  for (const child of node?.children ?? []) {
    const found = find(child, id);
    if (found) return found;
  }
  return null;
}
const narrow = () => (find(layout(), "sidebar")?.size?.px ?? Infinity) < 160;

function span(ms) {
  const m = Math.max(1, Math.round(ms / 60000));
  if (m < 60) return `${m}m`;
  const h = Math.floor(m / 60);
  if (h < 24) return `${h}h${String(m % 60).padStart(2, "0")}m`;
  return `${Math.floor(h / 24)}d${h % 24}h`;
}
const day = (iso) => { const d = new Date(iso); return `${MONTHS[d.getMonth()]} ${d.getDate()}`; };

const used = (a) => Math.round(a.usage?.usedPercent ?? 0);
const level = (pct, threshold) => (pct >= threshold ? "hot" : pct >= threshold - 20 ? "warn" : "ok");
const provider = (key) => PROVIDERS.find(p => p.key === key);

// Who an account is, as the person knows it: their own label, else the email
// they signed in with. agent-meter's ids (`claude-7`) are for the CLI.
const who = (a) => a?.label || a?.email || a?.id || "";
// Its plan as the provider sells it: "Max 20x", "Team", "Free".
const plan = (a) => {
  const p = (a?.plan ?? "").trim();
  if (!p || p === "-") return "";
  const m = a.capacityMultiplier > 1 ? ` ${a.capacityMultiplier}x` : "";
  return p[0].toUpperCase() + p.slice(1) + m;
};
const named = (a) => (plan(a) ? `${who(a)} (${plan(a)})` : who(a));

export default class AgentMeter extends View {
  init(_props, cx) {
    this.accounts = null;
    this.error = null;
    this.threshold = 90;
    this.choice = 0;
    const p = props() ?? {};
    if (checking() || p.dialog) return;
    this.refresh(cx);
    cx.timer.every(30000, (cx) => this.refresh(cx));
    // A switch or a fresh reading lands in agent-meter's own files: follow them.
    cx.spawn(async (cx) => {
      const where = (await exec("agent-meter", ["where"])).stdout.trim();
      if (!where) return;
      for (;;) {
        await watchFolder(where);
        this.refresh(cx);
      }
    });
  }

  refresh(cx) {
    cx.spawn(async (cx) => {
      const r = await exec("agent-meter", ["list", "--json"]);
      if (r.code !== 0) { this.error = r.stderr.trim() || "agent-meter list failed"; cx.notify(); return; }
      const next = JSON.parse(r.stdout);
      // The footer copy is always there, so it is the one that announces.
      if (props()?.compact) this.announce(this.accounts, next);
      this.accounts = next;
      this.error = null;
      if (props()?.compact) this.fit();
      cx.notify();
    });
  }

  // A switch made since the last reading becomes a notice.
  announce(before, after) {
    if (!before) return;
    for (const p of PROVIDERS) {
      const was = before.find(a => a.provider === p.key && a.active);
      const now = after.find(a => a.provider === p.key && a.active);
      if (!was || !now || was.id === now.id) continue;
      notify({
        // A toast holds one short line of each: the title says where it went, the
        // body what kind of account that is and why the old one was left.
        title: `Switched to ${who(now)}`,
        body: `${[p.short, plan(now)].filter(Boolean).join(" ")} · ${who(was)} hit ${used(was)}%`,
        level: "info",
        icon: p.icon,
        action: { name: "mod.open", args: { name: "agent-meter" } },
      });
    }
  }

  // One footer row per provider with an account in use: size the block to them.
  fit() {
    const rows = this.inUse().length;
    const px = rows ? rows * ROW + 8 : 0;
    if (find(layout(), "accounts")?.size?.px === px) return;
    run("layout.resize", { id: "accounts", px });
    run("layout.resize", { id: "sidebar-bottom", px: FOOTER + px });
  }

  inUse() {
    const list = this.accounts ?? [];
    return PROVIDERS.map(p => list.find(a => a.provider === p.key && a.active)).filter(Boolean);
  }

  colors(t) {
    return { ok: t.ansi2 ?? t.idle, warn: t.ansi3 ?? t.blocked, hot: t.ansi1 ?? t.blocked };
  }

  render() {
    const t = theme();
    const p = props() ?? {};
    if (p.dialog === "spend") return this.spendDialog(t, p);
    if (p.dialog === "menu") return this.menu(t, p);
    if (p.compact) return this.footer(t);
    return this.all(t, p);
  }

  // The footer: the account each provider is on now, with its fullest limit.
  footer(t) {
    const c = this.colors(t);
    const collapsed = narrow();
    const open = () => run("mod.open", { name: "agent-meter" });
    return v_flex().size_full().bg(t.chrome).px(8).pb(8).text_size(13)
      .when(collapsed, el => el.px(0).items_center())
      .children(this.inUse().map(a => {
        const pct = used(a);
        const lv = level(pct, this.threshold);
        const prov = provider(a.provider);
        const tip = `${prov.name}: ${named(a)}, ${pct}% used`;
        if (collapsed) {
          return div().id(`am-f-${a.id}`).relative().w(32).h(ROW).flex().items_center().justify_center().rounded(8)
            .hover(s => s.bg(t.hover)).cursor_pointer().tooltip(tip).on_click(open)
            .child(image(prov.icon).w(14).h(14))
            .child(div().absolute().top(5).right(6).size(6).rounded_full().bg(c[lv]));
        }
        return h_flex().id(`am-f-${a.id}`).h(ROW).px(8).gap(9).items_center().rounded(8)
          .text_color(t.muted).hover(s => s.bg(t.hover).text_color(t.foreground)).cursor_pointer()
          .tooltip(tip).on_click(open)
          .child(image(prov.icon).w(14).h(14).flex_none())
          .child(div().flex_1().min_w(0).truncate().child(who(a)))
          .child(div().flex_none().w(24).h(3).rounded(2).bg(t.border)
            .child(div().h_full().w(`${Math.min(100, Math.max(4, pct))}%`).rounded(2).bg(c[lv])))
          .child(div().flex_none().w(28).text_right().text_size(12).text_color(c[lv]).child(`${pct}%`));
      }));
  }

  // The modal: every account, and every limit it has.
  all(t, p) {
    const list = this.accounts ?? [];
    const live = agents();
    const c = this.colors(t);
    const now = Date.now();

    // One limit: its name, a bar, how full, and when it starts over.
    const limit = (a, w) => {
      const pct = Math.round(w.usedPercent);
      const color = w.counted ? c[level(pct, this.threshold)] : t.dim;
      return h_flex().h(18).gap(10).items_center().text_size(11.5)
        .when(!w.counted, el => el.tooltip("Not counted for switching"))
        .child(div().flex_none().w(96).truncate().text_color(w.counted ? t.muted : t.dim).child(w.label))
        .child(div().flex_1().h(4).rounded(2).bg(t.border)
          .child(div().h_full().w(`${Math.min(100, Math.max(1.5, pct))}%`).rounded(2).bg(color)))
        .child(div().flex_none().w(36).text_right().text_size(12).font_medium().text_color(color).child(`${pct}%`))
        .child(div().flex_none().w(104).text_color(t.dim)
          .when(Boolean(w.resetsAt), el => el.tooltip(`${day(w.resetsAt)}`).child(`resets in ${span(Date.parse(w.resetsAt) - now)}`)));
    };

    const row = (prov, a) => {
      const resets = a.usage?.limitResets;
      const on = a.active ? live.filter(x => prov.app.includes(x.app ?? "")) : [];
      const windows = a.usage?.windows ?? [];
      const notes = [
        resets?.available ? `${resets.available} limit reset${resets.available === 1 ? "" : "s"}${resets.nextExpiresAt ? `, the first expires ${day(resets.nextExpiresAt)}` : ""}` : null,
        a.disabled ? (a.disabled.until ? `left out of switching for ${span(Date.parse(a.disabled.until) - now)}` : "left out of switching") : null,
        a.needsLogin ? "needs signing in again" : null,
      ].filter(Boolean).join(" · ");
      return v_flex().id(`am-${a.id}`).px(10).py(7).gap(4).flex_none().rounded(8)
        .when(a.active, el => el.bg(t.selected))
        .cursor_pointer().hover(s => s.bg(t.hover))
        .on_click(() => this.openMenu(a, "pointer"))
        .child(h_flex().h(20).gap(8).items_center()
          .child(div().flex_none().max_w(230).truncate().text_color(a.active ? t.foreground : t.muted).font_medium().child(who(a)))
          .when(Boolean(plan(a)), el => el.child(div().flex_none().text_size(11.5).text_color(t.dim).child(plan(a))))
          .when(a.active, el => el.child(div().flex_none().px(6).h(17).rounded(5).bg(t.accent + "2e").text_size(10.5).font_semibold().text_color(t.accent).flex().items_center().child("In use")))
          .when(a.queue?.role === "next_up", el => el.child(div().flex_none().px(6).h(17).rounded(5).border(1).border_color(t.border).text_size(10.5).font_medium().text_color(t.muted).flex().items_center()
            .tooltip("Where agent-meter switches when this provider's account fills up").child("Next up")))
          .child(div().flex_1())
          .when(on.length > 0, el => el.child(h_flex().flex_none().gap(3).items_center().text_size(11).text_color(t.dim)
            .tooltip(on.map(x => `${x.space_name}${x.tab_name ? ` · ${x.tab_name}` : ""}`).join("\n"))
            .child(image(prov.icon).w(12).h(12)).child(`${on.length} agent${on.length === 1 ? "" : "s"}`)))
          .child(div().flex_none().text_size(11).text_color(t.dim).tooltip("agent-meter's id for it").child(a.id)))
        .when(windows.length === 0, el => el.child(div().text_size(11.5).text_color(t.dim).child(a.error ? `No reading: ${a.error}` : "No reading yet")))
        .children(windows.map(w => limit(a, w)))
        .when(Boolean(notes), el => el.child(div().pt(1).truncate().text_size(11.5).text_color(t.dim).child(notes)));
    };

    const group = (prov) => {
      // In the order agent-meter would switch through them, as its own UI lists them:
      // the one in use, the next one up, then the rest; one it can't use goes last.
      const place = (a) => a.queue?.position ?? Number.MAX_SAFE_INTEGER;
      const mine = list.filter(a => a.provider === prov.key).sort((x, y) => place(x) - place(y));
      if (!mine.length) return [];
      const room = mine.filter(a => used(a) < this.threshold && !a.disabled).length;
      return [
        h_flex().id(`am-h-${prov.key}`).px(10).pt(12).pb(4).flex_none().gap(7).items_center().text_size(11.5).font_medium()
          .child(image(prov.icon).w(14).h(14))
          .child(div().text_color(t.muted).child(prov.name))
          .child(div().flex_1())
          .child(div().text_color(t.dim).child(`${room} of ${mine.length} below ${this.threshold}%`)),
        ...mine.map(a => row(prov, a)),
      ];
    };

    return v_flex().size_full().min_h(0).bg(t.popover).text_size(13)
      .child(h_flex().px(18).h(48).flex_none().gap(10).items_center().border_b(1).border_color(t.border)
        .child(div().flex_1().text_size(14).font_semibold().text_color(t.foreground).child("Accounts"))
        .child(div().id("am-refresh").text_size(14).text_color(t.dim).cursor_pointer()
          .hover(s => s.text_color(t.foreground)).tooltip("Read again")
          .on_click((_e, cx) => this.refresh(cx)).child("↻"))
        .when(Boolean(p.overlay), el => el.child(div().id("am-close").text_color(t.dim).cursor_pointer()
          .hover(s => s.text_color(t.foreground)).tooltip("Close")
          .on_click(() => closeOverlay(p.overlay)).child(svg("mods/pane/close.svg").size(13)))))
      .when(Boolean(this.error), el => el.child(div().px(18).pt(10).text_size(12).text_color(t.blocked).child(this.error)))
      .when(this.accounts === null && !this.error, el => el.child(div().px(18).pt(10).text_size(12).text_color(t.dim).child("Reading accounts…")))
      .child(v_flex().id("am-list").flex_1().min_h(0).px(8).pb(10).overflow_y_scrollbar().children(PROVIDERS.flatMap(group)));
  }

  openMenu(a, at) {
    if (!a) return;
    openOverlay({ mod: "agent-meter", kind: "popup", at, width: 290, height: 176, props: { dialog: "menu", id: a.id, account: a } });
  }

  openSpend(a) {
    if (!a) return;
    openOverlay({ mod: "agent-meter", kind: "dialog", width: 480, height: 290, props: { dialog: "spend", id: a.id, account: a } });
  }

  // What can be done with one account.
  menu(t, p) {
    const a = p.account;
    const item = (id, text, onClick, dim = false) => h_flex().id(id).h(30).px(10).rounded(6).items_center().cursor_pointer()
      .text_color(dim ? t.dim : t.foreground).hover(s => s.bg(t.hover)).on_click(onClick).child(text);
    const close = () => closeOverlay(p.overlay);
    const resets = a?.usage?.limitResets?.available ?? 0;
    return v_flex().size_full().p(6).gap(1).text_size(13).bg(t.popover)
      .child(div().px(10).pt(4).pb(6).truncate().text_size(11).text_color(t.dim).child(named(a)))
      .child(item("m-use", a?.active ? "In use" : "Use this account", () => { close(); if (!a?.active) exec("agent-meter", ["use", p.id]); }, a?.active))
      .child(item("m-spend", resets ? `Spend a limit reset… (${resets})` : "No limit resets", () => { close(); if (resets) this.openSpend(a); }, !resets))
      .child(item("m-off5", "Leave out of switching for 5 hours", () => { close(); exec("agent-meter", ["disable", p.id, "--for", "5h"]); }))
      .child(item("m-off", a?.disabled ? "Take into switching again" : "Leave out until I take it back", () => {
        close(); exec("agent-meter", a?.disabled ? ["enable", p.id] : ["disable", p.id]);
      }));
  }

  // Spending a reset: say what it would do, then ask.
  spendDialog(t, p) {
    const a = p.account;
    const now = Date.now();
    const grants = (a?.usage?.limitResets?.grants ?? []).filter(g => g.id);
    const g = grants[Math.min(this.choice, grants.length - 1)];
    const windows = a?.usage?.windows ?? [];
    const refills = (g?.refills ?? []).map(kind => {
      // The limit of that kind for every model, as agent-meter itself names it.
      const w = windows.find(w => w.kind === kind && !w.scope) ?? windows.find(w => w.kind === kind);
      return w ? `${w.label} (now ${Math.round(w.usedPercent)}%)` : kind;
    });
    const icon = provider(a?.provider)?.icon ?? "icons/agent.svg";
    const button = (id, text, solid, onClick) => div().id(id).h(32).px(14).rounded(7).flex().items_center().cursor_pointer()
      .text_size(13).font_medium()
      .when(solid, el => el.bg(t.solid).text_color(t.solid_foreground))
      .when(!solid, el => el.border(1).border_color(t.border).text_color(t.foreground).hover(s => s.bg(t.hover)))
      .on_click(onClick).child(text);
    return v_flex().size_full().p(20).gap(12).bg(t.popover).text_size(13)
      .child(h_flex().gap(10).items_center()
        .child(image(icon).w(18).h(18).flex_none())
        .child(div().min_w(0).truncate().text_size(15).font_semibold().text_color(t.foreground).child(`Spend a limit reset on ${who(a)}?`)))
      .child(div().text_color(t.dim).child([provider(a?.provider)?.name, plan(a)].filter(Boolean).join(" · ")))
      .child(v_flex().gap(2).children(grants.map((x, i) => h_flex().id(`g-${i}`).h(28).px(8).gap(8).rounded(6).items_center().cursor_pointer()
        .when(x === g, el => el.bg(t.selected)).hover(s => s.bg(t.hover))
        .on_click((_e, cx) => { this.choice = i; cx.notify(); })
        .child(div().size(8).rounded(4).border(1).border_color(x === g ? t.accent : t.input).when(x === g, el => el.bg(t.accent)))
        .child(div().min_w(0).truncate().text_color(x === g ? t.foreground : t.muted).child(x.title)))))
      .when(Boolean(g), el => el.child(v_flex().gap(3).px(8).text_size(12.5).text_color(t.muted)
        .child(`Refills ${refills.join(" and ") || "every limit"}`)
        .when(Boolean(g.expiresAt), el => el.child(`Expires in ${span(Date.parse(g.expiresAt) - now)} (${day(g.expiresAt)})`))
        .when(g.usableNow === false, el => el.child(div().text_color(t.blocked).child(g.onlyAtLimit ? "Anthropic says it is usable only once a limit is reached" : "Anthropic says it is not usable right now")))))
      .child(div().flex_1())
      .child(h_flex().gap(8).items_center()
        .child(div().flex_1().text_size(12).text_color(t.dim).child("A reset is spent once and can't be undone."))
        .child(button("cancel", "Cancel", false, () => closeOverlay(p.overlay)))
        .child(button("spend", "Spend reset", true, () => {
          closeOverlay(p.overlay);
          exec("agent-meter", ["spend-reset", p.id, "--grant", g.id, "--yes"]).then(r => notify({
            title: r.code === 0 ? `Spent a limit reset on ${who(a)}` : `No reset was spent on ${who(a)}`,
            body: (r.stdout || r.stderr).trim().split("\n").at(-1),
            level: r.code === 0 ? "success" : "warning",
            icon,
          }));
        })));
  }
}
