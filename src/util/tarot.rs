//! 塔罗牌预置数据与数据库初始化（对应原版 util/TarotDataUtil）
//! 向 tarot 表写入 22 张大阿卡纳（带编号 number）

pub struct TarotItem {
    pub number: i64,
    pub name: &'static str,
    pub positive: &'static str,
    pub negative: &'static str,
}

/// 22 张大阿卡纳的标准解读数据
impl TarotItem {
    pub const fn new(
        number: i64,
        name: &'static str,
        positive: &'static str,
        negative: &'static str,
    ) -> TarotItem {
        TarotItem {
            number,
            name,
            positive,
            negative,
        }
    }
}

pub const PRESET: [TarotItem; 22] = [
    TarotItem::new(
        0,
        "愚者",
        "新的开始、冒险精神、无限可能，勇敢迈出第一步",
        "鲁莽冲动、犹豫不决、害怕改变而错失良机",
    ),
    TarotItem::new(
        1,
        "魔术师",
        "创造力、行动力、心想事成，把握手中的资源实现愿望",
        "欺骗与操纵、才华被埋没、方向错误导致事倍功半",
    ),
    TarotItem::new(
        2,
        "女祭司",
        "直觉敏锐、内在智慧、静观其变，倾听内心的声音",
        "忽视直觉、秘密与隐瞒、情绪波动看不清真相",
    ),
    TarotItem::new(
        3,
        "皇后",
        "丰饶滋养、母性光辉、爱与美的享受，收获丰硕成果",
        "过度依赖他人、不思进取、情感上的匮乏与停滞",
    ),
    TarotItem::new(
        4,
        "皇帝",
        "权威领导、稳定秩序、务实自律，建立坚实的规则",
        "独裁固执、控制欲过强、僵化不知变通",
    ),
    TarotItem::new(
        5,
        "教皇",
        "传统传承、精神指引、贵人相助，遵循可靠的准则",
        "教条主义、过度保守、因循守旧拒绝新思想",
    ),
    TarotItem::new(
        6,
        "恋人",
        "爱情甜蜜、心意相通、重要的选择，追随内心所爱",
        "关系失衡、分离与考验、在抉择中摇摆不定",
    ),
    TarotItem::new(
        7,
        "战车",
        "意志坚定、勇往直前、克服障碍取得胜利",
        "失控与鲁莽、方向迷失、被情绪左右而停滞",
    ),
    TarotItem::new(
        8,
        "力量",
        "温柔而坚定、耐心与勇气、以柔克刚战胜困难",
        "自我怀疑、意志薄弱、被恐惧与焦虑压垮",
    ),
    TarotItem::new(
        9,
        "隐士",
        "内省沉淀、独处思考、寻求真理，点亮自己的灯",
        "过度孤立、逃避现实、固执己见拒绝帮助",
    ),
    TarotItem::new(
        10,
        "命运之轮",
        "时来运转、命运转机、顺势而为，把握周期性变化",
        "厄运缠身、抗拒变化、错失转运的时机",
    ),
    TarotItem::new(
        11,
        "正义",
        "公正平衡、因果报应、诚实负责，是非自有公断",
        "不公与偏见、逃避责任、判断失误受到反噬",
    ),
    TarotItem::new(
        12,
        "倒吊人",
        "以退为进、换位思考、暂时的沉淀，等待时机成熟",
        "无谓牺牲、拖延不决、钻牛角尖看不清全局",
    ),
    TarotItem::new(
        13,
        "死神",
        "结束与蜕变、放下过去、焕然新生，迎接新的阶段",
        "抗拒结束、停滞不前、旧事物迟迟不肯放手",
    ),
    TarotItem::new(
        14,
        "节制",
        "调和适度、耐心沟通、取长补短，达到平衡状态",
        "失衡极端、操之过急、资源浪费难以持续",
    ),
    TarotItem::new(
        15,
        "恶魔",
        "欲望与束缚、沉迷诱惑、看清执念才能挣脱枷锁",
        "挣脱束缚、看清真相、拒绝诱惑重获自由",
    ),
    TarotItem::new(
        16,
        "高塔",
        "突如其来的剧变、打破旧局、真相大白后的觉醒",
        "灾难暂缓、逃避现实、固执己见终致崩塌",
    ),
    TarotItem::new(
        17,
        "星星",
        "希望与疗愈、灵感涌现、心怀信念，前路光明",
        "希望破灭、信心不足、暂时迷失方向",
    ),
    TarotItem::new(
        18,
        "月亮",
        "迷茫与潜意识、梦境与幻觉、警惕不实的信息",
        "拨云见日、真相大白、不安逐渐消散",
    ),
    TarotItem::new(
        19,
        "太阳",
        "成功喜悦、活力充沛、坦诚乐观，一切欣欣向荣",
        "暂时阴霾、自满轻敌、小挫折不必灰心",
    ),
    TarotItem::new(
        20,
        "审判",
        "觉醒重生、反省过往、接受召唤，迎来崭新开始",
        "自我怀疑、错失良机、沉溺于过去的悔恨",
    ),
    TarotItem::new(
        21,
        "世界",
        "圆满完成、目标达成、功德圆满，开启新的循环",
        "功亏一篑、停滞不前、尚有未竟之事待处理",
    ),
];

/// 确保 tarot 表已初始化
pub fn ensure_initialized() {
    if crate::db::dao::count_tarot() > 0 {
        return;
    }
    for item in PRESET.iter() {
        crate::db::dao::insert_tarot(item.number, item.name, item.positive, item.negative);
    }
    crate::runtime::log::info(format!(
        "塔罗牌数据初始化完成, 已写入 {} 张牌",
        PRESET.len()
    ));
}
