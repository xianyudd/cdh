fn main() {
    std::process::exit(cdh::controller::run());
}
















// use std::io::{self, Stderr, Write};
// use std::time::Duration;

// use crossterm::{
//     cursor::{Hide, MoveTo, Show},
//     event::{self, Event, KeyCode, KeyEvent},
//     style::{Attribute, Print},
//     terminal::{
//         disable_raw_mode, enable_raw_mode, size, Clear, ClearType,
//         EnterAlternateScreen, LeaveAlternateScreen,
//     },
//     QueueableCommand,
// };
// use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

// /// -------- RAII：确保异常退出也能恢复终端 --------
// struct Guard {
//     err: Stderr,
// }
// impl Guard {
//     fn new() -> io::Result<Self> {
//         let mut err = io::stderr();
//         enable_raw_mode()?;
//         err.queue(EnterAlternateScreen)?
//             .queue(Hide)?
//             .flush()?;
//         Ok(Self { err })
//     }
//     fn restore(mut self) -> io::Result<()> {
//         self.err
//             .queue(Show)?
//             .queue(LeaveAlternateScreen)?
//             .flush()?;
//         disable_raw_mode()
//     }
// }
// impl Drop for Guard {
//     fn drop(&mut self) {
//         // 在 Drop 中做同样的恢复操作，保证 panic 时也能还原
//         let _ = self.err.queue(Show);
//         let _ = self.err.queue(LeaveAlternateScreen);
//         let _ = self.err.flush();
//         let _ = disable_raw_mode();
//     }
// }

// /// -------- 工具函数 --------
// fn strip_ansi(s: &str) -> String {
//     let mut plain = String::with_capacity(s.len());
//     let mut bytes = s.as_bytes().iter().copied();
//     while let Some(b) = bytes.next() {
//         if b == 0x1B {
//             // 跳过 ESC 开头的 CSI / OSC / DCS 等序列
//             if let Some(b2) = bytes.next() {
//                 match b2 {
//                     b'[' => {
//                         // CSI ——一直读到 0x40..0x7E 结束符
//                         while let Some(x) = bytes.next() {
//                             if (0x40..=0x7E).contains(&x) {
//                                 break;
//                             }
//                         }
//                     }
//                     b']' => {
//                         // OSC ——读到 BEL 或 ESC \
//                         let mut prev = 0;
//                         while let Some(x) = bytes.next() {
//                             if x == 0x07 || (prev == 0x1B && x == b'\\') {
//                                 break;
//                             }
//                             prev = x;
//                         }
//                     }
//                     b'P' => {
//                         // DCS ——读到 ESC \
//                         let mut prev = 0;
//                         while let Some(x) = bytes.next() {
//                             if prev == 0x1B && x == b'\\' {
//                                 break;
//                             }
//                             prev = x;
//                         }
//                     }
//                     _ => {}
//                 }
//             }
//         } else {
//             plain.push(b as char);
//         }
//     }
//     plain
// }
// fn clip_middle(s: &str, w: usize) -> String {
//     let txt = strip_ansi(s);
//     if txt.width() <= w {
//         txt
//     } else if w == 1 {
//         "…".into()
//     } else {
//         let mut left = String::new();
//         let mut right = String::new();
//         let mut lw = 0;
//         let mut rw = 0;
//         for ch in txt.chars() {
//             let cw = ch.width().unwrap_or(1);
//             if lw + cw + 1 + rw <= w {
//                 left.push(ch);
//                 lw += cw;
//             } else {
//                 break;
//             }
//         }
//         for ch in txt.chars().rev() {
//             let cw = ch.width().unwrap_or(1);
//             if lw + 1 + rw + cw <= w {
//                 right.insert(0, ch);
//                 rw += cw;
//             } else {
//                 break;
//             }
//         }
//         format!("{left}…{right}")
//     }
// }

// /// -------- 绘制底部面板 --------
// fn draw(
//     err: &mut Stderr,
//     term_w: u16,
//     term_h: u16,
//     ph: u16,
//     items: &[String],
//     page: usize,
//     per_page: usize,
//     sel: usize,
// ) -> io::Result<()> {
//     // 先整屏清空，再只画面板部分即可（备用屏幕内，别怕闪）
//     err.queue(MoveTo(0, 0))?
//         .queue(Clear(ClearType::All))?;

//     let top = term_h.saturating_sub(ph);

//     // 顶边
//     err.queue(MoveTo(0, top))?;
//     if ph >= 2 {
//         err.queue(Print("╭".to_string()))?;
//         if term_w > 2 {
//             err.queue(Print("─".repeat(term_w as usize - 2)))?;
//         }
//         err.queue(Print("╮".to_string()))?;
//     }

//     // 内容
//     let (body_rows, content_start) = if ph >= 2 { (ph - 2, top + 1) } else { (1, top) };
//     let start = page * per_page;
//     let end = std::cmp::min(start + per_page, items.len());

//     for i in 0..body_rows {
//         let row = content_start + i;
//         err.queue(MoveTo(0, row))?;
//         let idx = start + i as usize;
//         let label = if idx < end { &items[idx] } else { "" };

//         if ph >= 2 {
//             let inner = term_w.saturating_sub(2) as usize;
//             let clipped = clip_middle(label, inner);
//             let pad = inner.saturating_sub(clipped.width());
//             let mut line = String::from("│");
//             line.push_str(&clipped);
//             line.push_str(&" ".repeat(pad));
//             line.push('│');

//             if idx == start + sel {
//                 err.queue(Print(Attribute::Reverse))?;
//                 err.queue(Print(line))?;
//                 err.queue(Print(Attribute::NoReverse))?;
//             } else {
//                 err.queue(Print(line))?;
//             }
//         } else {
//             // 单行面板
//             let clipped = clip_middle(label, term_w as usize);
//             if idx == start + sel {
//                 err.queue(Print(Attribute::Reverse))?;
//                 err.queue(Print(clipped))?;
//                 err.queue(Print(Attribute::NoReverse))?;
//             } else {
//                 err.queue(Print(clipped))?;
//             }
//         }
//     }

//     // 底边
//     if ph >= 2 {
//         let bot = top + ph - 1;
//         err.queue(MoveTo(0, bot))?;
//         err.queue(Print("╰".to_string()))?;
//         if term_w > 2 {
//             err.queue(Print("─".repeat(term_w as usize - 2)))?;
//         }
//         err.queue(Print("╯".to_string()))?;
//     }

//     err.flush()
// }

// /// -------- UI 事件循环 --------
// fn run_ui(items: Vec<String>, req_h: u16) -> io::Result<Option<String>> {
//     let guard = Guard::new()?; // 进入备用屏

//     let (w, h) = size()?;
//     let ph = req_h.min(h.saturating_sub(1)).max(1);

//     let per_page = if ph >= 2 { ph - 2 } else { 1 } as usize;
//     let mut page = 0usize;
//     let total_pages = std::cmp::max(1, (items.len() + per_page - 1) / per_page);
//     let mut sel_in_page = 0usize;
//     let mut digit_buf: Option<usize> = None;

//     draw(&mut io::stderr(), w, h, ph, &items, page, per_page, sel_in_page)?;

//     loop {
//         if !event::poll(Duration::from_millis(300))? {
//             continue;
//         }
//         match event::read()? {
//             Event::Key(KeyEvent { code, .. }) => {
//                 match code {
//                     KeyCode::Esc | KeyCode::Char('q') => {
//                         guard.restore()?; // 正常退出
//                         return Ok(None);
//                     }
//                     KeyCode::Up | KeyCode::Char('k') => {
//                         if sel_in_page > 0 {
//                             sel_in_page -= 1;
//                         } else if page > 0 {
//                             page -= 1;
//                             let vis = std::cmp::min(per_page, items.len() - page * per_page);
//                             sel_in_page = vis - 1;
//                         }
//                     }
//                     KeyCode::Down | KeyCode::Char('j') => {
//                         let vis = std::cmp::min(per_page, items.len() - page * per_page);
//                         if sel_in_page + 1 < vis {
//                             sel_in_page += 1;
//                         } else if page + 1 < total_pages {
//                             page += 1;
//                             sel_in_page = 0;
//                         }
//                     }
//                     KeyCode::Left | KeyCode::Char('p') => {
//                         if page > 0 {
//                             page -= 1;
//                             let vis = std::cmp::min(per_page, items.len() - page * per_page);
//                             sel_in_page = sel_in_page.min(vis - 1);
//                         }
//                     }
//                     KeyCode::Right | KeyCode::Char('n') => {
//                         if page + 1 < total_pages {
//                             page += 1;
//                             let vis = std::cmp::min(per_page, items.len() - page * per_page);
//                             sel_in_page = sel_in_page.min(vis - 1);
//                         }
//                     }
//                     KeyCode::Char(c) if c.is_ascii_digit() => {
//                         digit_buf = Some((c as u8 - b'0') as usize);
//                     }
//                     KeyCode::Enter => {
//                         let start = page * per_page;
//                         let vis = std::cmp::min(per_page, items.len() - start);
//                         let pick_idx = if let Some(d) = digit_buf.take() {
//                             if d < vis { start + d } else { continue }
//                         } else {
//                             start + sel_in_page
//                         };
//                         let picked = items[pick_idx].clone();
//                         guard.restore()?;
//                         return Ok(Some(picked));
//                     }
//                     _ => {}
//                 }
//                 draw(&mut io::stderr(), w, h, ph, &items, page, per_page, sel_in_page)?;
//             }
//             Event::Resize(new_w, new_h) => {
//                 // 重新计算并重画
//                 let (w, h) = (new_w, new_h);
//                 draw(&mut io::stderr(), w, h, ph, &items, page, per_page, sel_in_page)?;
//             }
//             _ => {}
//         }
//     }
// }

// fn main() -> io::Result<()> {
//     let args: Vec<String> = std::env::args().skip(1).collect();
//     let items = if args.is_empty() {
//         (0..30).map(|i| format!("Demo item {i}")).collect()
//     } else {
//         args
//     };

//     let req_h: u16 = std::env::var("CDH_HEIGHT")
//         .ok()
//         .and_then(|s| s.parse().ok())
//         .unwrap_or(12);

//     match run_ui(items, req_h)? {
//         Some(picked) => println!("{picked}"),
//         None => {}
//     }
//     Ok(())
// }
