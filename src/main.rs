// src/main.rs
fn main() {
    // 初始化全局上下文：里面有 Paths（路径）和 Config（配置），以后还能继续扩展
    let ctx = cdh::AppContext::init_from_process();

    // 把上下文传给 controller，让 controller 不再自己管路径细节
    std::process::exit(cdh::controller::run(&ctx));
}
