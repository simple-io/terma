//! 终端控制字符

use std::convert::Into;
use std::string::ToString;

// 终端控制字符枚举
enum ControlChar {
    C0(),
    CSI(u16, char),
}

/// CSI  终端控制字符, 格式：ESC[<参数><最终字节>，分段解释：
///     ESC[：固定前缀，表示为 CSI 控制字符开始
///     <参数>：可选参数，控制字符的参数，通常是一个或多个数字，使用逗号分割，省略参数默认值为 1
///     <最终字节>：为控制字符的最终字节，表示控制字符的用途
static SOH: ControlChar = ControlChar::CSI(0x01, 'A'); // 向上移动光标
static STX: ControlChar = ControlChar::CSI(0x02, 'B'); // 向下移动光标
static ETX: ControlChar = ControlChar::CSI(0x03, 'C'); // 向右移动光标
static EOT: ControlChar = ControlChar::CSI(0x04, 'D'); // 向左移动光标
static ENQ: ControlChar = ControlChar::CSI(0x05, 'E'); // 请求确认
static ACK: ControlChar = ControlChar::CSI(0x06, 'F'); // 确认
static BEL: ControlChar = ControlChar::CSI(0x07, 'G'); // 响铃
static BS: ControlChar = ControlChar::CSI(0x08, 'H'); // 退格
static HT: ControlChar = ControlChar::CSI(0x09, 'I'); // 水平制表符
static LF: ControlChar = ControlChar::CSI(0x0A, 'J'); // 换行
static VT: ControlChar = ControlChar::CSI(0x0B, 'K'); // 垂直制表符
static FF: ControlChar = ControlChar::CSI(0x0C, 'L'); // 换页
static CR: ControlChar = ControlChar::CSI(0x0D, 'M'); // 回车
static SO: ControlChar = ControlChar::CSI(0x0E, 'N'); // 单字节数据开始
static SI: ControlChar = ControlChar::CSI(0x0F, 'O'); // 单字节数据结束
static VPA: ControlChar = ControlChar::CSI(0x64, 'd'); // 垂直移动光标