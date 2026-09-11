//! 独立模式（onebot standalone）：命令分发与服务注册
pub mod api;
pub mod commands;
pub mod dispatcher;

#[cfg(test)]
mod dist_test;

#[cfg(test)]
mod ten_pull_test;

#[cfg(test)]
mod trainer_test;
