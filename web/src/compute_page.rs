//! The compute grid: all-time statistics of the phones that think for the
//! bots. Headline totals as stat tiles, then a donor leaderboard grouped by
//! player + device (someone's phone and laptop count separately).

use leptos::prelude::*;
use leptos::task::spawn_local;
use snek_shared::protocol::ComputeOverview;

fn fmt_sims(sims: u64) -> String {
    match sims {
        0..=999 => sims.to_string(),
        1_000..=999_999 => format!("{:.1}k", sims as f64 / 1e3),
        1_000_000..=999_999_999 => format!("{:.1}M", sims as f64 / 1e6),
        _ => format!("{:.2}B", sims as f64 / 1e9),
    }
}

fn fmt_time(ms: u64) -> String {
    match ms {
        0..=999 => format!("{ms}ms"),
        1_000..=59_999 => format!("{:.1}s", ms as f64 / 1e3),
        60_000..=3_599_999 => format!("{:.1}min", ms as f64 / 60e3),
        _ => format!("{:.1}h", ms as f64 / 3600e3),
    }
}

fn fmt_rate(sims: u64, ms: u64) -> String {
    if ms == 0 {
        return "—".to_string();
    }
    format!("{}k sims/s", sims / ms) // sims/ms == k sims/s
}

pub fn compute_page() -> impl IntoView {
    let overview = RwSignal::new(Option::<ComputeOverview>::None);
    let failed = RwSignal::new(false);

    spawn_local(async move {
        match gloo_net::http::Request::get("/api/compute").send().await {
            Ok(resp) => match resp.json::<ComputeOverview>().await {
                Ok(o) => overview.set(Some(o)),
                Err(_) => failed.set(true),
            },
            Err(_) => failed.set(true),
        }
    });

    view! {
        <div>
            <h2 style="margin-top: 22px;">"🧠 botnet"</h2>
            <p class="footnote">
                "the bot engine is run in the browser by whoever has the "
                "fastest device as you play"
            </p>
            {move || {
                if failed.get() {
                    return view! { <div class="banner error">"Couldn't load stats."</div> }
                        .into_any();
                }
                let Some(o) = overview.get() else {
                    return view! { <div class="banner">"Counting simulations…"</div> }.into_any();
                };
                if o.totals.moves == 0 {
                    return view! {
                        <div class="banner">
                            "No bot games yet. Add a bot to a game and your device "
                            "becomes part of the grid."
                        </div>
                    }
                        .into_any();
                }
                let donors = o.donors.clone();
                view! {
                    <div class="stat-grid">
                        <div class="stat">
                            <div class="stat-value">{fmt_sims(o.totals.sims)}</div>
                            <div class="footnote">"simulations donated"</div>
                        </div>
                        <div class="stat">
                            <div class="stat-value">{fmt_time(o.totals.wall_ms)}</div>
                            <div class="footnote">"of thinking time"</div>
                        </div>
                        <div class="stat">
                            <div class="stat-value">{o.totals.moves.to_string()}</div>
                            <div class="footnote">"bot moves computed"</div>
                        </div>
                        <div class="stat">
                            <div class="stat-value">{o.totals.devices.to_string()}</div>
                            <div class="footnote">
                                "devices across " {o.totals.games} " games"
                            </div>
                        </div>
                    </div>

                    <h3 style="margin-top: 24px;">"top donors"</h3>
                    <table class="players">
                        <tbody>
                            {donors
                                .into_iter()
                                .enumerate()
                                .map(|(i, d)| {
                                    let medal = ["🥇", "🥈", "🥉"].get(i).copied().unwrap_or("🧠");
                                    view! {
                                        <tr>
                                            <td>
                                                {medal} " " {d.name}
                                                <br />
                                                <span class="footnote">{d.device}</span>
                                            </td>
                                            <td class="right">
                                                {fmt_sims(d.sims)} " sims"
                                                <br />
                                                <span class="footnote">
                                                    {d.moves} " moves · " {d.games} " games · "
                                                    {fmt_rate(d.sims, d.wall_ms)}
                                                </span>
                                            </td>
                                        </tr>
                                    }
                                })
                                .collect_view()}
                        </tbody>
                    </table>
                }
                    .into_any()
            }}
            <p style="margin-top: 16px;">
                <a href="#/">"← home"</a>
            </p>
        </div>
    }
}
