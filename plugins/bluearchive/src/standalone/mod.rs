//! 独立模式（onebot standalone）：命令分发与服务注册
pub mod api;
pub mod commands;
pub mod dispatcher;
pub mod history;

#[cfg(test)]
mod onebot_print_test;
#[cfg(test)]
mod ten_pull_test;

#[cfg(test)]
mod trainer_test;
