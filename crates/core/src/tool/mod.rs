pub mod read;
pub mod search_file;
pub mod write;

// 工作区路径解析与安全策略属于工具实现细节，暂不对外暴露。
pub(crate) mod workspace;
