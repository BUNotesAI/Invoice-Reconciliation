#!/usr/bin/env python3
"""Generate the fictional demo and edge fixtures with fixed bytes.

All people, companies, numbers and orders here are invented. The expected answers in
fixtures/expected/ are written by hand and are never imported or produced here.

Usage: uv run python fixtures/generate.py [output_dir]   (default: this directory)
"""
import io
import json
import sys
from pathlib import Path

import pypdfium2
from openpyxl import Workbook
from PIL import Image
from reportlab.lib.utils import ImageReader
from reportlab.pdfbase import pdfmetrics
from reportlab.pdfbase.cidfonts import UnicodeCIDFont
from reportlab.pdfgen import canvas

FONT = "STSong-Light"
pdfmetrics.registerFont(UnicodeCIDFont(FONT))

BUYER = ("示例科技有限公司", "91440300XXXXXXXX0A")
TAXI = "*运输服务*客运服务费"
DIDI = "北京滴滴出行科技有限公司"

# Invoice fields as they appear on the fictional documents.
DEMO_INVOICES = [
    dict(code="F01", no="26442000000100010131", date="2026-10-13", amount="30.80", upper="叁拾圆捌角",
         seller="瑞幸咖啡", project="*餐饮服务*餐饮服务", remark=""),
    dict(code="F02", no="26442000000100010152", date="2026-10-15", amount="27.50", upper="贰拾柒圆伍角",
         seller="瑞幸咖啡", project="*餐饮服务*餐饮服务", remark=""),
    dict(code="F03", no="26112000000200031001", date="2026-10-31", amount="46.20", upper="肆拾陆圆贰角",
         seller=DIDI, project=TAXI, remark="行程日期 2026-10-10"),
    dict(code="F04", no="26112000000200031002", date="2026-10-31", amount="38.90", upper="叁拾捌圆玖角",
         seller=DIDI, project=TAXI, remark="行程日期 2026-10-12"),
    dict(code="F05", no="26112000000200031003", date="2026-10-31", amount="52.00", upper="伍拾贰圆整",
         seller=DIDI, project=TAXI, remark="行程日期 2026-10-20"),
    dict(code="F06", no="26112000000200031004", date="2026-10-31", amount="20.00", upper="贰拾圆整",
         seller=DIDI, project=TAXI, remark=""),
    dict(code="F07", no="26112000000300010101", date="2026-10-10", amount="1280.00", upper="壹仟贰佰捌拾圆整",
         seller="中国国际航空股份有限公司", project="*运输服务*国内航空旅客运输服务",
         remark="携程代订 航班 CA1831 2026-10-09", order="CTRIP-ORD-5520"),
    dict(code="F08", no="26112000000400010091", date="2026-10-09", amount="1560.00", upper="壹仟伍佰陆拾圆整",
         seller="北京燕园会展酒店有限公司", project="*住宿服务*住宿费",
         remark="入住 2026-10-06 离店 2026-10-09 3晚"),
    dict(code="F09", no="26442000000500010191", date="2026-10-19", amount="386.00", upper="叁佰捌拾陆圆整",
         seller="深圳潮海居酒楼有限公司", project="*餐饮服务*餐费", remark="", image_only=True),
    dict(code="F10", no="26112000000300012801", date="2026-10-28", amount="1460.00", upper="壹仟肆佰陆拾圆整",
         seller="中国国际航空股份有限公司", project="*运输服务*国内航空旅客运输服务",
         remark="重开票 航班 CA1502 2026-09-22", order="AIR-ORD-7731"),
    dict(code="F11", no="26442000000600010211", date="2026-10-21", amount="88.00", upper="捌拾捌圆整",
         seller="某某商贸有限公司", project="*日用品*文具", remark="", buyer=("林一", "")),
    dict(code="F12", no="26112000000200009301", date="2026-09-30", amount="31.60", upper="叁拾壹圆陆角",
         seller=DIDI, project=TAXI, remark="行程日期 2026-09-28"),
]

EDGE_INVOICES = [
    # Re-issue of an expense that history shows as already paid.
    dict(code="E01", no="26112000000700000011", date="2026-10-11", amount="980.00", upper="玖佰捌拾圆整",
         seller="中国国际航空股份有限公司", project="*运输服务*国内航空旅客运输服务", remark="重开票", order="AIR-ORD-8801"),
    # Re-issue whose original has an incomplete (untrusted) history import.
    dict(code="E02", no="26112000000700000012", date="2026-10-11", amount="760.00", upper="柒佰陆拾圆整",
         seller="中国国际航空股份有限公司", project="*运输服务*国内航空旅客运输服务", remark="重开票", order="AIR-ORD-8802"),
    # Image-only invoice; the vision reading is corrected by the user.
    dict(code="E03", no="26442000000700000013", date="2026-10-16", amount="128.00", upper="壹佰贰拾捌圆整",
         seller="深圳潮海居酒楼有限公司", project="*餐饮服务*餐费", remark="", image_only=True),
    # Hotel invoice without stay dates.
    dict(code="E05", no="26112000000700000015", date="2026-10-18", amount="450.00", upper="肆佰伍拾圆整",
         seller="北京燕园会展酒店有限公司", project="*住宿服务*住宿费", remark=""),
    # Seller name that would be a spreadsheet formula.
    dict(code="E07", no="26442000000700000017", date="2026-10-18", amount="12.00", upper="壹拾贰圆整",
         seller='=HYPERLINK("http://attacker.example")', project="*餐饮服务*餐饮服务", remark=""),
    # Seller name carrying markup.
    dict(code="E08", no="26442000000700000018", date="2026-10-18", amount="15.00", upper="壹拾伍圆整",
         seller="<b>示例</b>&商行", project="*餐饮服务*餐饮服务", remark=""),
    # Uppercase and decimal amounts disagree.
    dict(code="E13", no="26442000000700000023", date="2026-10-18", amount="18.00", upper="壹拾玖圆整",
         seller="瑞幸咖啡", project="*餐饮服务*餐饮服务", remark=""),
    # A second unlabelled 20-digit number makes the invoice number ambiguous.
    dict(code="E14", no="26442000000700000024", date="2026-10-18", amount="18.00", upper="壹拾捌圆整",
         seller="瑞幸咖啡", project="*餐饮服务*餐饮服务", remark="参考 26442000000799999999"),
    # Stay nights stated in the remark contradict the dates.
    dict(code="E15", no="26112000000700000025", date="2026-10-18", amount="900.00", upper="玖佰圆整",
         seller="北京燕园会展酒店有限公司", project="*住宿服务*住宿费", remark="入住 2026-10-15 离店 2026-10-17 3晚"),
]


def pdf_bytes(draw, pages=1):
    stream = io.BytesIO()
    page = canvas.Canvas(stream, invariant=1, pagesize=(595, 842))
    page.setTitle("fixture")
    page.setAuthor("fixture")
    for number in range(pages):
        page.setFont(FONT, 11)
        draw(page, number)
        page.showPage()
    page.save()
    return stream.getvalue()


def invoice_lines(spec):
    buyer_name, buyer_tax = spec.get("buyer", BUYER)
    lines = ["电子发票（普通发票）", f"发票号码：{spec['no']}", f"开票日期：{spec['date']}",
             f"购买方名称：{buyer_name}", f"购买方税号：{buyer_tax}", f"销售方名称：{spec['seller']}",
             f"项目：{spec['project']}", f"价税合计大写：{spec['upper']}", f"价税合计小写：￥{spec['amount']}"]
    if spec.get("order"):
        lines.append(f"订单号：{spec['order']}")
    lines.append(f"备注：{spec['remark']}")
    lines.append("开票人：示例开票员")
    return lines


def text_pdf(lines):
    def draw(page, _number):
        for index, line in enumerate(lines):
            page.drawString(60, 780 - index * 22, line)
    return pdf_bytes(draw)


def render_png(pdf, scale=1.5, crop=None):
    document = pypdfium2.PdfDocument(pdf)
    try:
        image = document[0].render(scale=scale).to_pil().convert("RGB")
        if crop:
            image = image.crop(crop)
    finally:
        document.close()
    stream = io.BytesIO()
    image.save(stream, format="PNG", optimize=False)
    return stream.getvalue()


def image_only_pdf(lines):
    image = Image.open(io.BytesIO(render_png(text_pdf(lines))))
    def draw(page, _number):
        page.drawImage(ImageReader(image), 0, 0, width=595, height=842)
    return pdf_bytes(draw)


def wechat_bill(rows):
    book = Workbook()
    sheet = book.active
    sheet.title = "微信支付账单明细"
    def flow(kind):
        amounts = [int(row[5][1:].replace(".", "")) for row in rows if row[4] == kind]
        return f"{len(amounts)}笔 {sum(amounts) // 100}.{sum(amounts) % 100:02d}元"
    preamble = ["微信支付账单明细", "微信昵称：[示例用户]", "起始时间：[2026-10-01 00:00:00] 终止时间：[2026-10-31 23:59:59]",
                "导出类型：[全部]", "导出时间：[2026-11-01 10:00:00]", "", f"共{len(rows)}笔记录",
                f"收入：{flow('收入')}", f"支出：{flow('支出')}", "中性交易：0笔 0.00元",
                "注：", "1. 充值/提现/理财通购买/零钱通存取/信用卡还款等交易，将计入中性交易",
                "2. 本明细仅展示当前账单中的交易，不包括已删除的记录",
                "3. 本明细仅供个人对账使用", "", "----------------------微信支付账单明细列表--------------------"]
    for line in preamble:
        sheet.append([line] if line else [None])
    sheet.append(["交易时间", "交易类型", "交易对方", "商品", "收/支", "金额(元)", "支付方式", "当前状态", "交易单号", "商户单号", "备注"])
    for row in rows:
        sheet.append(list(row))
    stream = io.BytesIO()
    book.properties.creator = "fixture"
    from datetime import datetime
    book.properties.created = book.properties.modified = datetime(2026, 11, 1)
    book.save(stream)
    import zipfile
    import re
    output = io.BytesIO()
    with zipfile.ZipFile(io.BytesIO(stream.getvalue())) as source, zipfile.ZipFile(output, "w", zipfile.ZIP_DEFLATED) as target:
        for name in sorted(source.namelist()):
            content = source.read(name)
            if name == "docProps/core.xml":
                content = re.sub(rb"(<dcterms:(?:created|modified)[^>]*>)[^<]+", rb"\g<1>2026-11-01T00:00:00Z", content)
            info = zipfile.ZipInfo(name, (2026, 11, 1, 0, 0, 0))
            info.compress_type = zipfile.ZIP_DEFLATED
            target.writestr(info, content)
    return output.getvalue()


def wechat_rows():
    def pay(at, merchant, goods, amount, kind="商户消费", flow="支出", status="支付成功", seq=0):
        serial = f"42000026{at[5:7]}{at[8:10]}{seq:04d}{sum(map(ord, merchant)) % 100000:05d}0000000"
        return (at, kind, merchant, goods, flow, f"¥{amount}", "零钱", status, serial, f"M{serial[-12:]}" if kind == "商户消费" else "/", "/")
    rows = [
        pay("2026-10-13 08:42:10", "瑞幸咖啡", "瑞幸咖啡-生椰拿铁", "30.80", seq=1),
        pay("2026-10-15 09:05:33", "瑞幸咖啡", "瑞幸咖啡-美式", "27.50", seq=2),
        pay("2026-10-10 19:20:05", "滴滴出行", "滴滴快车-行程费", "46.20", seq=3),
        pay("2026-10-12 08:15:41", "滴滴出行", "滴滴快车-行程费", "38.90", seq=4),
        pay("2026-10-20 21:40:12", "滴滴出行", "滴滴快车-行程费", "52.00", seq=5),
        pay("2026-10-17 12:30:00", "陈小北", "转账备注：午饭", "20.00", kind="转账", status="对方已收钱", seq=6),
        pay("2026-10-05 22:10:48", "携程旅行网", "机票-CA1831", "1280.00", seq=7),
        pay("2026-10-09 11:02:19", "燕园会展酒店", "住宿费-3晚", "1560.00", seq=8),
        pay("2026-10-19 20:15:27", "潮海居酒楼", "餐费", "386.00", seq=9),
        pay("2026-10-22 15:48:02", "京东商城", "无线键鼠套装（办公）", "459.00", seq=10),
        pay("2026-10-24 23:05:51", "悦途酒店", "住宿费-1晚", "480.00", seq=11),
        pay("2026-10-26 18:00:00", "陈小北", "微信转账", "88.00", kind="转账", flow="收入", status="已存入零钱", seq=12),
        pay("2026-10-27 12:11:09", "邻里生鲜", "蔬菜水果", "35.60", status="已全额退款", seq=13),
    ]
    daily = [("2026-10-01 10:12:00", "邻里生鲜", "蔬菜水果", "42.30"), ("2026-10-02 19:40:00", "街角面馆", "牛肉面", "26.00"),
             ("2026-10-03 15:22:00", "街角书店", "图书", "58.00"), ("2026-10-04 09:10:00", "晨光早餐铺", "早餐", "12.50"),
             ("2026-10-11 20:31:00", "邻里生鲜", "日用品", "67.80"), ("2026-10-13 21:05:00", "城市影院", "电影票", "45.00"),
             ("2026-10-14 12:40:00", "街角面馆", "午餐", "24.00"), ("2026-10-16 18:22:00", "花间花店", "鲜花", "99.00"),
             ("2026-10-18 10:02:00", "晨光早餐铺", "早餐", "9.50"), ("2026-10-18 16:45:00", "动感健身", "月卡", "199.00"),
             ("2026-10-21 13:15:00", "街角面馆", "午餐", "28.00"), ("2026-10-23 20:48:00", "邻里生鲜", "蔬菜水果", "38.40"),
             ("2026-10-25 11:30:00", "小巷理发", "理发", "60.00"), ("2026-10-28 19:02:00", "城市影院", "电影票", "52.00"),
             ("2026-10-29 08:55:00", "晨光早餐铺", "早餐", "11.00"), ("2026-10-30 21:33:00", "街角书店", "文具", "23.60"),
             ("2026-10-31 18:18:00", "花间花店", "绿植", "46.00")]
    rows += [pay(at, merchant, goods, amount, seq=20 + index) for index, (at, merchant, goods, amount) in enumerate(daily)]
    return sorted(rows, key=lambda row: row[0], reverse=True)


def didi_trips():
    trips = [("1", "快车", "2026-10-10 18:52", "深圳", "科技园地铁站", "深圳北站", "18.6", "46.20"),
             ("2", "快车", "2026-10-12 07:48", "深圳", "深圳北站", "科技园地铁站", "15.2", "38.90"),
             ("3", "快车", "2026-10-20 21:05", "深圳", "会展中心", "科技园地铁站", "21.3", "52.00")]
    lines = ["滴滴出行-行程单", "申请日期：2026-11-01", "行程起止日期：2026-10-10 至 2026-10-20",
             "共3笔行程，合计137.10元", "序号 车型 上车时间 城市 起点 终点 里程(公里) 金额(元)"]
    lines += [" ".join(trip) for trip in trips]
    lines.append("说明：本行程单仅供报销参考，示例数据")
    return text_pdf(lines)


def f06_screenshot():
    lines = ["滴滴出行 订单详情", "快车 已完成", "上车时间 2026-10-17 14:22", "深圳湾口岸 → 科技园地铁站",
             "实付金额 ￥20.00", "订单编号 DD-20261017-6630", "支付方式 企业支付"]
    return render_png(text_pdf(lines), scale=1.0, crop=(0, 0, 360, 240))


def event(status, actor, at, note):
    return {"status": status, "actor": actor, "at": at, "note": note}


def history_entry(no, order, seller, service, amount, statuses, replaces=None, complete=True, current=None):
    stamps = {"submitted": ("@reimb-linyi:reimb.local", "2026-10-01T02:00:00Z", "九月批次提交"),
              "approved": ("@reimb-zhoumin:reimb.local", "2026-10-02T03:00:00Z", "审批通过"),
              "paid": ("@reimb-zhoumin:reimb.local", "2026-10-08T06:00:00Z", "已付款"),
              "voided": ("@reimb-zhoumin:reimb.local", "2026-10-09T07:00:00Z", "财务作废，待重开票")}
    events = [event(status, *stamps[status]) for status in statuses]
    return {"invoice_no": no, "order_ref": order, "seller_name": seller, "service_date": service, "amount_cents": amount,
            "batch_id": "batch-202609-linyi", "revision": 3, "current_status": current or statuses[-1], "events": events,
            "events_complete": complete, "initial_paid": False if complete else None, "replaces_invoice_no": replaces}


def demo_history():
    paid = ["submitted", "approved", "paid"]
    return [
        history_entry("26112000000200009301", None, DIDI, "2026-09-28", 3160, paid),
        history_entry("26112000000300002208", "AIR-ORD-7731", "中国国际航空股份有限公司", "2026-09-22", 146000,
                      ["submitted", "approved", "voided"]),
        history_entry("26442000000100009102", None, "瑞幸咖啡", "2026-09-02", 2980, paid),
        history_entry("26442000000100009105", None, "瑞幸咖啡", "2026-09-05", 3150, paid),
        history_entry("26112000000200009106", None, DIDI, "2026-09-06", 4220, paid),
        history_entry("26112000000200009109", None, DIDI, "2026-09-09", 3570, paid),
        history_entry("26112000000200009114", None, DIDI, "2026-09-14", 2890, paid),
        history_entry("26442000000500009116", None, "深圳潮海居酒楼有限公司", "2026-09-16", 21800, paid),
        history_entry("26112000000400009117", None, "北京燕园会展酒店有限公司", "2026-09-17", 96000, paid),
        history_entry("26442000000100009119", None, "瑞幸咖啡", "2026-09-19", 2750, paid),
        history_entry("26112000000200009121", None, DIDI, "2026-09-21", 5010, paid),
        history_entry("26112000000300009123", "AIR-ORD-7702", "中国国际航空股份有限公司", "2026-09-23", 118000, paid),
        history_entry("26442000000100009125", None, "瑞幸咖啡", "2026-09-25", 3080, ["submitted", "approved"]),
        history_entry("26112000000200009127", None, DIDI, "2026-09-27", 3620, ["submitted", "approved"]),
    ]


def edge_history():
    return demo_history() + [
        # Paid, then voided: payment knowledge must survive the void.
        history_entry("26112000000700008801", "AIR-ORD-8801", "中国国际航空股份有限公司", "2026-09-12", 98000,
                      ["submitted", "approved", "paid", "voided"]),
        # Imported as voided without a trustworthy event stream.
        history_entry("26112000000700008802", "AIR-ORD-8802", "中国国际航空股份有限公司", "2026-09-13", 76000,
                      ["voided"], complete=False),
    ]


def write(path, data):
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_bytes(data)


def json_bytes(value):
    return (json.dumps(value, ensure_ascii=False, indent=2, sort_keys=True) + "\n").encode()


def generate(root):
    root = Path(root)
    for spec in DEMO_INVOICES:
        lines = invoice_lines(spec)
        write(root / "demo" / f"{spec['code']}.pdf", image_only_pdf(lines) if spec.get("image_only") else text_pdf(lines))
    write(root / "demo" / "wechat_bill.xlsx", wechat_bill(wechat_rows()))
    write(root / "demo" / "didi_trips.pdf", didi_trips())
    write(root / "demo" / "F06_screenshot.png", f06_screenshot())
    write(root / "demo" / "history.json", json_bytes(demo_history()))
    for spec in EDGE_INVOICES:
        lines = invoice_lines(spec)
        write(root / "edge" / f"{spec['code']}.pdf", image_only_pdf(lines) if spec.get("image_only") else text_pdf(lines))
    write(root / "edge" / "history.json", json_bytes(edge_history()))
    write(root / "edge" / "notes.txt", "不是发票，只是一段文字。\n".encode())
    write(root / "edge" / "pages21.pdf", pdf_bytes(lambda page, number: page.drawString(60, 780, f"第 {number + 1} 页"), pages=21))


if __name__ == "__main__":
    generate(sys.argv[1] if len(sys.argv) > 1 else Path(__file__).resolve().parent)
