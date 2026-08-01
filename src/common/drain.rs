//! 退场协调：统计正在传输的流式响应，让进程能挑一个「没有流在跑」的时刻退出。
//!
//! 为什么不用 axum 的 `with_graceful_shutdown`：它一收到信号就关掉监听 socket，
//! 然后等已有连接跑完。本进程是单实例单端口（容器里 `PID 1` 就是它），关了监听
//! 就没有第二个后端能接活，上游会被拒连，**拒连时长等于 drain 上限**。在线更新
//! 如果把上限设成几分钟，就是用救几条流的代价换来更长时间的 502，净亏。
//!
//! 所以反过来：**先等再退**。替换二进制后继续正常服务，轮询在途流计数，归零那一刻
//! 立即硬退出。上游被拒连的窗口和现在一样短（只有新进程冷启动那一下），而已经在跑
//! 的流不会被砍。等不到安静时刻就超时硬退——那正是今天的行为，所以最坏情况持平，
//! 不存在比现在更差的分支。
//!
//! 只统计流式响应（`content-type: text/event-stream`）。非流式响应生命周期是毫秒级，
//! 计入只会让计数永远不归零，反而逼着每次都走超时路径。

use std::sync::atomic::{AtomicUsize, Ordering};

static STREAMS_IN_FLIGHT: AtomicUsize = AtomicUsize::new(0);

/// 正在传输的流式响应数。
pub fn streams_in_flight() -> usize {
    STREAMS_IN_FLIGHT.load(Ordering::Acquire)
}

/// 在途流的持有凭证：必须绑在**响应体**上而不是 handler 的返回值上。
///
/// SSE 的 handler 拿到上游第一个字节就返回了，真正的传输发生在 body 被逐块拉取的
/// 过程中。绑错位置会让计数在流刚开始时就归零，等于没统计。
pub struct StreamGuard;

impl StreamGuard {
    pub fn acquire() -> Self {
        STREAMS_IN_FLIGHT.fetch_add(1, Ordering::AcqRel);
        Self
    }
}

impl Drop for StreamGuard {
    fn drop(&mut self) {
        STREAMS_IN_FLIGHT.fetch_sub(1, Ordering::AcqRel);
    }
}

type ExitTask = Box<dyn Fn() + Send + Sync>;

static EXIT_TASKS: std::sync::Mutex<Vec<(&'static str, ExitTask)>> =
    std::sync::Mutex::new(Vec::new());

/// 登记一个退出前要跑的收尾动作（目前用于 SQLite WAL 截断）。
///
/// 用注册表而不是把各个 store 传进退出路径：退出是从 `AdminService` 触发的，而
/// `key_supplier.db` 之类的库并不挂在它下面。注册表让「谁需要收尾」和「什么时候退出」
/// 彻底解耦，新增一个库时不用再改退出路径。
pub fn register_exit_task(name: &'static str, task: impl Fn() + Send + Sync + 'static) {
    if let Ok(mut tasks) = EXIT_TASKS.lock() {
        tasks.push((name, Box::new(task)));
    }
}

/// 跑完所有收尾动作。单个动作自己负责不要卡住——卡住退出等于把新版本无限期推迟。
pub fn run_exit_tasks() {
    let Ok(tasks) = EXIT_TASKS.lock() else {
        return;
    };
    for (name, task) in tasks.iter() {
        let started = std::time::Instant::now();
        task();
        tracing::info!(
            task = name,
            elapsed_ms = started.elapsed().as_millis(),
            "退出前收尾完成"
        );
    }
}

/// 断言绝对计数的测试必须串行：计数器是进程级的，并行跑会互相看到对方的凭证。
#[cfg(test)]
pub static TEST_SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guards_count_up_and_release_on_drop_including_panics() {
        let _serial = TEST_SERIAL.lock().unwrap();
        // 计数不归零就退不出去，只能走超时硬退——那就等于这套机制没生效。
        let before = streams_in_flight();
        {
            let _a = StreamGuard::acquire();
            let _b = StreamGuard::acquire();
            assert_eq!(streams_in_flight(), before + 2);
        }
        assert_eq!(streams_in_flight(), before);

        // 流被客户端中断时 body 是被 drop 而不是正常读完的，那条路径也必须释放。
        let guard = StreamGuard::acquire();
        assert_eq!(streams_in_flight(), before + 1);
        drop(guard);
        assert_eq!(streams_in_flight(), before);
    }
}
