// agent-meter for the Herdr app: every subscription account, how much of it is
// used, which of your agents run on it, and the switches the watcher makes.
//
// Prototype. Everything comes from `agent-meter list --json`; actions run the
// agent-meter CLI. Nothing here polls a provider.

import { View, div, image, svg } from "gpui-kit";
import { v_flex, h_flex } from "gpui-base";
import { agents, exec, run, theme, props, openOverlay, closeOverlay, notify, watchFolder, checking } from "herdr";

const PROVIDERS = [
  { key: "claude", name: "Claude Code", app: ["claude-code", "claude"], icon: "icons/claude.svg" },
  { key: "codex", name: "Codex", app: ["codex"], icon: "icons/codex.svg" },
];

const MONTHS = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];

function span(ms) {
  const m = Math.max(1, Math.round(ms / 60000));
  if (m < 60) return `${m}m`;
  const h = Math.floor(m / 60);
  if (h < 24) return `${h}h${String(m % 60).padStart(2, "0")}m`;
  return `${Math.floor(h / 24)}d${h % 24}h`;
}

const used = (a) => Math.round(a.usage?.usedPercent ?? 0);
// The window that decides how full the account is: the fullest one it is judged on.
const top = (a) => [...(a.usage?.windows ?? [])].filter(w => w.counted).sort((x, y) => y.usedPercent - x.usedPercent)[0];
const level = (pct, threshold) => (pct >= threshold ? "hot" : pct >= threshold - 20 ? "warn" : "ok");

export default class AgentMeter extends View {
  init(_props, cx) {
    this.accounts = null;
    this.error = null;
    this.threshold = 90;
    this.hovered = null;
    this.busy = null;
    this.choice = 0;
    if (checking()) return;
    if (props()?.dialog) return;
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
      this.announce(this.accounts, next);
      this.accounts = next;
      this.error = null;
      cx.notify();
    });
  }

  // A switch the watcher made since the last reading becomes a notice.
  announce(before, after) {
    if (!before) return;
    for (const p of PROVIDERS) {
      const was = before.find(a => a.provider === p.key && a.active);
      const now = after.find(a => a.provider === p.key && a.active);
      if (!was || !now || was.id === now.id) continue;
      notify({
        title: `${p.name} moved to ${now.id}`,
        body: `${was.id} reached ${used(was)}% · ${now.id} is at ${used(now)}%`,
        level: "info",
        icon: p.icon,
        action: { name: "mod.open", args: { name: "agent-meter" } },
      });
    }
  }

  render(cx) {
    const t = theme();
    const p = props() ?? {};
    if (p.dialog === "spend") return this.spendDialog(t, p);
    if (p.dialog === "menu") return this.menu(t, p);
    return this.panel(t);
  }

  colors(t) {
    return { ok: t.ansi2 ?? t.idle, warn: t.ansi3 ?? t.blocked, hot: t.ansi1 ?? t.blocked };
  }

  panel(t) {
    const list = this.accounts ?? [];
    const live = agents();
    const c = this.colors(t);
    const now = Date.now();

    const agentsOn = (prov, account) =>
      account.active ? live.filter(a => prov.app.includes(a.app ?? "")) : [];

    const row = (prov, a) => {
      const pct = used(a);
      const lv = level(pct, this.threshold);
      const w = top(a);
      const resets = a.usage?.limitResets?.available ?? 0;
      const on = agentsOn(prov, a);
      const hovered = this.hovered === a.id;
      const quiet = !a.active && !hovered;
      const detail = [
        w?.resetsAt ? `${w.label} limit resets in ${span(Date.parse(w.resetsAt) - now)}` : null,
        resets ? `${resets} limit reset${resets === 1 ? "" : "s"}` : null,
        a.disabled ? (a.disabled.until ? `left out for ${span(Date.parse(a.disabled.until) - now)}` : "left out") : null,
      ].filter(Boolean).join(" · ");
      return v_flex()
        .id(`am-${a.id}`)
        .px(8).py(5).gap(4).flex_none().rounded(8)
        .when(!a.active && Boolean(detail), el => el.tooltip(detail))
        .when(a.active, el => el.bg(t.selected))
        .cursor_pointer()
        .hover(s => s.bg(t.hover))
        .on_hover((inside, cx) => { this.hovered = inside ? a.id : (this.hovered === a.id ? null : this.hovered); cx.notify(); })
        .on_click(() => openOverlay({ mod: "agent-meter", kind: "popup", at: "pointer", width: 250, height: 196, props: { dialog: "menu", id: a.id, account: a } }))
        .child(h_flex().gap(8).items_center()
          .child(div().min_w(0).truncate().text_color(quiet ? t.muted : t.foreground).child(a.label ?? a.id))
          .when(a.active, el => el.child(div().flex_none().px(6).h(17).rounded(5).bg(t.accent + "2e").text_size(10.5).font_semibold().text_color(t.accent).flex().items_center().child("In use")))
          .child(div().flex_1())
          .when(on.length > 0, el => el.child(h_flex().flex_none().gap(3).items_center().text_size(11).text_color(t.dim)
            .tooltip(on.map(x => `${x.space_name}${x.tab_name ? ` · ${x.tab_name}` : ""}`).join("\n"))
            .child(image(prov.icon).w(12).h(12))
            .child(String(on.length))))
          .child(div().flex_none().w(34).text_right().text_size(12).font_medium().text_color(c[lv]).child(`${pct}%`)))
        .child(div().h(3).w_full().rounded(2).bg(t.border)
          .child(div().h_full().w(`${Math.min(100, Math.max(2, pct))}%`).rounded(2).bg(c[lv])))
        .when(a.active && Boolean(detail), el => el.child(div().truncate().text_size(11).text_color(t.dim).child(detail)));
    };

    const group = (prov) => {
      const mine = list.filter(a => a.provider === prov.key);
      if (!mine.length) return [];
      const room = mine.filter(a => used(a) < this.threshold && !a.disabled).length;
      return [
        h_flex().id(`am-h-${prov.key}`).px(8).pt(10).h(32).flex_none().gap(6).items_center().text_size(11).font_medium()
          .child(image(prov.icon).w(13).h(13))
          .child(div().text_color(t.dim).child(prov.name))
          .child(div().flex_1())
          .child(div().text_color(t.dim).child(`${room} of ${mine.length} below ${this.threshold}%`)),
        ...mine.map(a => row(prov, a)),
      ];
    };

    return v_flex().size_full().min_h(0).bg(t.chrome).border_t(1).border_color(t.border).text_size(13)
      .child(h_flex().px(16).h(41).pt(14).pb(5).flex_none().items_center()
        .child(div().flex_1().text_size(11.5).font_medium().text_color(t.dim).child("Accounts"))
        .child(div().id("am-refresh").text_size(11).text_color(t.dim).cursor_pointer()
          .hover(s => s.text_color(t.foreground)).tooltip("Read again")
          .on_click((_e, cx) => this.refresh(cx)).child(div().text_size(14).child("↻")))
        // Opened as a window (a key, a toast's click): it closes itself.
        .when(Boolean(props()?.overlay), el => el.child(div().id("am-close").ml(10).text_color(t.dim).cursor_pointer()
          .hover(s => s.text_color(t.foreground)).tooltip("Close")
          .on_click(() => closeOverlay(props().overlay)).child(svg("mods/pane/close.svg").size(12)))))
      .when(Boolean(this.error), el => el.child(div().px(16).text_size(12).text_color(t.blocked).child(this.error)))
      .when(this.accounts === null && !this.error, el => el.child(div().px(16).text_size(12).text_color(t.dim).child("Reading accounts…")))
      .child(v_flex().id("am-list").flex_1().min_h(0).px(8).pb(8).overflow_y_scrollbar().children(PROVIDERS.flatMap(group)));
  }

  // The account menu: what can be done with one account.
  menu(t, p) {
    const a = p.account;
    const item = (id, text, onClick, dim = false) => h_flex().id(id).h(30).px(10).rounded(6).items_center().cursor_pointer()
      .text_color(dim ? t.dim : t.foreground).hover(s => s.bg(t.hover)).on_click(onClick).child(text);
    const close = () => closeOverlay(p.overlay);
    const resets = a?.usage?.limitResets?.available ?? 0;
    return v_flex().size_full().p(6).gap(1).text_size(13).bg(t.popover)
      .child(div().px(10).pt(4).pb(6).text_size(11).text_color(t.dim).child(a ? `${a.id} · ${a.email}` : p.id))
      .child(item("m-use", a?.active ? "In use" : "Use this account", () => { close(); if (!a?.active) exec("agent-meter", ["use", p.id]); }, a?.active))
      .child(item("m-spend", resets ? `Spend a limit reset… (${resets})` : "No limit resets", () => {
        close();
        if (resets) openOverlay({ mod: "agent-meter", kind: "dialog", width: 470, height: 290, props: { dialog: "spend", id: p.id, account: a } });
      }, !resets))
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
    const button = (id, text, solid, onClick) => div().id(id).h(32).px(14).rounded(7).flex().items_center().cursor_pointer()
      .text_size(13).font_medium()
      .when(solid, el => el.bg(t.solid).text_color(t.solid_foreground))
      .when(!solid, el => el.border(1).border_color(t.border).text_color(t.foreground).hover(s => s.bg(t.hover)))
      .on_click(onClick).child(text);
    return v_flex().size_full().p(20).gap(12).bg(t.popover).text_size(13)
      .child(h_flex().gap(10).items_center()
        .child(image(PROVIDERS.find(x => x.key === a?.provider)?.icon ?? "icons/agent.svg").w(18).h(18))
        .child(div().text_size(15).font_semibold().text_color(t.foreground).child(`Spend a limit reset on ${p.id}?`)))
      .child(div().text_color(t.dim).child(a?.email ?? ""))
      .child(v_flex().gap(2).children(grants.map((x, i) => h_flex().id(`g-${i}`).h(28).px(8).gap(8).rounded(6).items_center().cursor_pointer()
        .when(x === g, el => el.bg(t.selected)).hover(s => s.bg(t.hover))
        .on_click((_e, cx) => { this.choice = i; cx.notify(); })
        .child(div().size(8).rounded(4).border(1).border_color(x === g ? t.accent : t.input).when(x === g, el => el.bg(t.accent)))
        .child(div().min_w(0).truncate().text_color(x === g ? t.foreground : t.muted).child(x.title)))))
      .when(Boolean(g), el => el.child(v_flex().gap(3).px(8).text_size(12.5).text_color(t.muted)
        .child(`Refills ${refills.join(" and ") || "every limit"}`)
        .when(Boolean(g.expiresAt), el => el.child(`Expires in ${span(Date.parse(g.expiresAt) - now)} (${MONTHS[new Date(g.expiresAt).getMonth()]} ${new Date(g.expiresAt).getDate()})`))
        .when(g.usableNow === false, el => el.child(div().text_color(t.blocked).child(g.onlyAtLimit ? "Anthropic says it is usable only once a limit is reached" : "Anthropic says it is not usable right now")))))
      .child(div().flex_1())
      .child(h_flex().gap(8).items_center()
        .child(div().flex_1().text_size(12).text_color(t.dim).child("A reset is spent once and can't be undone."))
        .child(button("cancel", "Cancel", false, () => closeOverlay(p.overlay)))
        .child(button("spend", "Spend reset", true, () => {
          closeOverlay(p.overlay);
          exec("agent-meter", ["spend-reset", p.id, "--grant", g.id, "--yes"]).then(r => notify({
            title: r.code === 0 ? `Spent a limit reset on ${p.id}` : `No reset was spent on ${p.id}`,
            body: (r.stdout || r.stderr).trim().split("\n").at(-1),
            level: r.code === 0 ? "success" : "warning",
            icon: "icons/claude.svg",
          }));
        })));
  }
}
