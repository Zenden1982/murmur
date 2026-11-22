import sys, re
from fontTools.ttLib import TTFont
from fontTools.fontBuilder import FontBuilder
from fontTools.pens.ttGlyphPen import TTGlyphPen
from fontTools.pens.cu2quPen import Cu2QuPen
from fontTools.pens.transformPen import TransformPen
from fontTools.svgLib.path import parse_path

phdir, cps_rs, tsv, out_dir = sys.argv[1:5]
cps = {m.group(1): int(m.group(2), 16) for m in re.finditer(r'pub const ([A-Z0-9_]+): &str = "\\u\{([0-9A-F]+)\}"', open(cps_rs, encoding='utf8').read())}
outl = dict(l.split('\t', 1) for l in open(tsv, encoding='utf8').read().splitlines() if l.strip())

REG = {'PLUS':'add','X':'close','HASH':'text-channel','SPEAKER_HIGH':'voice-channel','SPEAKER_SLASH':'deafen','MICROPHONE':'mic',
 'MICROPHONE_SLASH':'mic-off','HEADPHONES':'headphones','MONITOR_ARROW_UP':'screen-share','CURSOR_CLICK':'remote-control','PHONE_X':'hang-up',
 'GEAR_SIX':'settings','SLIDERS_HORIZONTAL':'settings','USER_PLUS':'invite','SHIELD_CHECK':'roles','MAGNIFYING_GLASS':'search','PAPER_PLANE_RIGHT':'send',
 'DOTS_THREE':'more','PAPERCLIP':'attach','SMILEY':'emoji','BELL':'bell','PUSH_PIN':'pin','VIDEO_CAMERA':'camera','SIGN_OUT':'leave','MINUS_CIRCLE':'dnd',
 'WAVEFORM':'sound-on-wave','USER_SOUND':'speaking','SPEAKER_SIMPLE_HIGH':'volume'}
FILL = {'MICROPHONE':'mic','MICROPHONE_SLASH':'mic-off','HEADPHONES':'headphones','SPEAKER_HIGH':'voice-channel','SPEAKER_SIMPLE_HIGH':'volume','SPEAKER_SLASH':'deafen',
 'MONITOR_ARROW_UP':'screen-share','PHONE_X':'hang-up','GEAR_SIX':'settings','USER_PLUS':'invite','SIGN_OUT':'leave','SHIELD':'roles','HASH':'text-channel',
 'PLUS':'add','X':'close'}

def calib(font_path):
    f = TTFont(font_path)
    cmap = f.getBestCmap(); gs = f.getGlyphSet(); glyf = f['glyf']
    g = glyf[cmap[cps['PLUS']]]; g.recalcBounds(glyf)
    # плюс phosphor: линии 40..216 сетки 256, толщина 16 → края 32..224
    a = (g.xMax - g.xMin) / 192.0; bx = g.xMin - a * 32
    ay = (g.yMax - g.yMin) / 192.0; by = g.yMax + ay * 32  # y_font = by - ay*y256
    adv = f['hmtx'][cmap[cps['PLUS']]][0]
    return f, a, bx, ay, by, adv

def build(font_path, mapping, name, out):
    src, a, bx, ay, by, adv = calib(font_path)
    upm = src['head'].unitsPerEm
    k = 256.0 / 24.0  # сетка 24 → 256
    order = ['.notdef']; glyphs = {}; cmap = {}; metrics = {}
    pen0 = TTGlyphPen(None); glyphs['.notdef'] = pen0.glyph(); metrics['.notdef'] = (adv, 0)
    for ph, icon in mapping.items():
        if ph not in cps or icon not in outl: print('нет', ph, icon); continue
        gname = 'g_' + ph.lower()
        pen = TTGlyphPen(None)
        tp = TransformPen(Cu2QuPen(pen, 1.0, reverse_direction=True), (a * k, 0, 0, -ay * k, bx, by))
        parse_path(outl[icon], tp)
        g = pen.glyph()
        order.append(gname); glyphs[gname] = g; cmap[cps[ph]] = gname
        metrics[gname] = (adv, 0)
    fb = FontBuilder(upm, isTTF=True)
    fb.setupGlyphOrder(order); fb.setupCharacterMap(cmap); fb.setupGlyf(glyphs)
    for gn in order:
        g = glyphs[gn]
        if hasattr(g, 'recalcBounds'): g.recalcBounds(fb.font['glyf'])
        metrics[gn] = (adv, getattr(g, 'xMin', 0) or 0)
    fb.setupHorizontalMetrics(metrics)
    hh = src['hhea']
    fb.setupHorizontalHeader(ascent=hh.ascent, descent=hh.descent)
    fb.setupNameTable({'familyName': name, 'styleName': 'Regular'})
    os2 = src['OS/2']
    fb.setupOS2(sTypoAscender=os2.sTypoAscender, sTypoDescender=os2.sTypoDescender, usWinAscent=os2.usWinAscent, usWinDescent=os2.usWinDescent)
    fb.setupPost()
    fb.save(out)
    print(name, len(order) - 1, 'глифов, upm', upm, 'adv', adv)

build(phdir + 'Phosphor.ttf', REG, 'MurmurIcons', out_dir + '/murmur-icons.ttf')
build(phdir + 'Phosphor-Fill.ttf', FILL, 'MurmurIconsFill', out_dir + '/murmur-icons-fill.ttf')
