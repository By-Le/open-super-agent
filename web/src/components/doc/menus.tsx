// 斜杠菜单（打 / 或中文输入法下的 、）和选中文字时的格式菜单。菜单项都是 Markdown 存得下的格式。
import { useEffect, useLayoutEffect, useRef, useState } from 'react'
import { Extension, type Editor } from '@tiptap/core'
import { PluginKey } from '@tiptap/pm/state'
import Suggestion from '@tiptap/suggestion'
import { BubbleMenu } from '@tiptap/react/menus'
import { t } from '../../i18n'

export type SlashItem = { key: string; label: string; icon: string; kw: string; run: (e: Editor) => void }
export type SlashState = { items: SlashItem[]; rect: DOMRect | null; pick: (it: SlashItem) => void } | null

// 关键词：英文和拼音首字母，中文不在这里（界面文字都要过 t()）
export const slashItems = (askImage: () => void): SlashItem[] => [
  { key: 'p', icon: 'T', label: t('正文'), kw: 'text paragraph zw', run: e => e.chain().focus().setParagraph().run() },
  { key: 'h1', icon: 'H1', label: t('标题 1'), kw: 'h1 heading bt', run: e => e.chain().focus().setHeading({ level: 1 }).run() },
  { key: 'h2', icon: 'H2', label: t('标题 2'), kw: 'h2 heading bt', run: e => e.chain().focus().setHeading({ level: 2 }).run() },
  { key: 'h3', icon: 'H3', label: t('标题 3'), kw: 'h3 heading bt', run: e => e.chain().focus().setHeading({ level: 3 }).run() },
  { key: 'ul', icon: '•', label: t('无序列表'), kw: 'bullet list ul wxlb', run: e => e.chain().focus().toggleBulletList().run() },
  { key: 'ol', icon: '1.', label: t('有序列表'), kw: 'ordered number list ol yxlb', run: e => e.chain().focus().toggleOrderedList().run() },
  { key: 'todo', icon: '☐', label: t('待办'), kw: 'todo task check db', run: e => e.chain().focus().toggleTaskList().run() },
  { key: 'quote', icon: '❝', label: t('引用'), kw: 'quote blockquote yy', run: e => e.chain().focus().toggleBlockquote().run() },
  { key: 'callout', icon: 'ℹ︎', label: t('提示框'), kw: 'callout note tip tsk', run: e => e.chain().focus().wrapIn('callout', { kind: 'NOTE' }).run() },
  { key: 'toggle', icon: '▸', label: t('折叠块'), kw: 'toggle details fold zdk', run: e => e.chain().focus().setDetails().run() },
  { key: 'code', icon: '</>', label: t('代码块'), kw: 'code dmk', run: e => e.chain().focus().toggleCodeBlock().run() },
  { key: 'table', icon: '▦', label: t('表格'), kw: 'table bg', run: e => e.chain().focus().insertTable({ rows: 3, cols: 3, withHeaderRow: true }).run() },
  { key: 'hr', icon: '—', label: t('分割线'), kw: 'divider hr line fgx', run: e => e.chain().focus().setHorizontalRule().run() },
  { key: 'image', icon: '🖼', label: t('图片'), kw: 'image picture tp', run: () => askImage() },   // i18n-ignore
]

type Storage = { show: (s: SlashState) => void; key: (e: KeyboardEvent) => boolean; items: () => SlashItem[] }

export const SlashCommands = Extension.create<object, Storage>({
  name: 'slashCommands',
  addStorage() { return { show: () => {}, key: () => false, items: () => [] } },
  addProseMirrorPlugins() {
    const storage = this.storage
    // / 要在段首或空格后面打（a/b 这种网址不弹）；、是中文里常用的顿号，只在段首打才弹
    const make = (char: string, name: string, startOfLine: boolean) => Suggestion<SlashItem, SlashItem>({
      editor: this.editor,
      char,
      pluginKey: new PluginKey(name),
      allowSpaces: false,
      startOfLine,
      allow: ({ state, range }) => state.doc.resolve(range.from).parent.type.name === 'paragraph',
      items: ({ query }) => {
        const q = query.toLowerCase()
        return storage.items().filter(it => !q || it.label.toLowerCase().includes(q) || it.kw.includes(q))
      },
      command: ({ editor, range, props }) => {
        editor.chain().focus().deleteRange(range).run()
        props.run(editor)
      },
      render: () => {
        const show = (p: { items: SlashItem[]; clientRect?: (() => DOMRect | null) | null; command: (it: SlashItem) => void }) =>
          storage.show(p.items.length ? { items: p.items, rect: p.clientRect?.() ?? null, pick: p.command } : null)
        return {
          onStart: show,
          onUpdate: show,
          onExit: () => storage.show(null),
          onKeyDown: ({ event }) => {
            if (event.key === 'Escape') { storage.show(null); return true }
            return storage.key(event)
          },
        }
      },
    })
    return [make('/', 'slash', false), make('、', 'slash-zh', true)]
  },
})

/** 斜杠菜单：贴着光标显示，下面放不下就往上翻。 */
export function SlashMenu({ editor }: { editor: Editor }) {
  const [state, setState] = useState<SlashState>(null)
  const [cur, setCur] = useState(0)
  const box = useRef<HTMLDivElement>(null)
  const ref = useRef({ state, cur })
  ref.current = { state, cur }
  useEffect(() => {
    const st = (editor.storage as any).slashCommands as Storage | undefined
    if (!st) return   // 编辑器已经销毁了
    st.show = s => { setState(s); setCur(0) }
    st.key = e => {
      const { state: s, cur: c } = ref.current
      if (!s) return false
      if (e.key === 'ArrowDown') { setCur((c + 1) % s.items.length); return true }
      if (e.key === 'ArrowUp') { setCur((c - 1 + s.items.length) % s.items.length); return true }
      if (e.key === 'Enter' || e.key === 'Tab') { s.pick(s.items[c]); return true }
      return false
    }
    return () => { st.show = () => {}; st.key = () => false }
  }, [editor])
  useLayoutEffect(() => { box.current?.querySelector('.on')?.scrollIntoView({ block: 'nearest' }) }, [cur, state])
  if (!state?.rect) return null
  const vh = window.visualViewport?.height ?? window.innerHeight
  const up = state.rect.bottom + 300 > vh && state.rect.top > 300
  const style = up ? { left: state.rect.left, bottom: vh - state.rect.top + 4 } : { left: state.rect.left, top: state.rect.bottom + 4 }
  return (
    <div className="slash-menu" ref={box} style={style} data-no-swipe="" onMouseDown={e => e.preventDefault()}>
      {state.items.map((it, k) => (
        <button key={it.key} className={k === cur ? 'on' : ''} onMouseEnter={() => setCur(k)} onClick={() => state.pick(it)}>
          <span className="slash-icon">{it.icon}</span>{it.label}
        </button>
      ))}
    </div>
  )
}

const LINK_OK = /^(https?:\/\/|mailto:|\/(?!\/)|#)/i
export const safeHref = (href: string) => {
  const h = href.trim()
  if (!h) return ''
  return LINK_OK.test(h) ? h : /^[\w-]+(\.[\w-]+)+/.test(h) ? 'https://' + h : ''
}

/** 选中文字时的格式菜单：粗体、斜体、删除线、行内代码、链接。 */
export function Toolbar({ editor }: { editor: Editor }) {
  const [link, setLink] = useState<string | null>(null)
  const apply = () => {
    const href = safeHref(link || '')
    if (href) editor.chain().focus().extendMarkRange('link').setLink({ href }).run()
    else editor.chain().focus().extendMarkRange('link').unsetLink().run()
    setLink(null)
  }
  return (
    <BubbleMenu editor={editor} className="doc-toolbar" data-no-swipe=""
      shouldShow={({ editor: e, state }) => e.isEditable && !state.selection.empty && !e.isActive('codeBlock') && !e.isActive('rawBlock')}
      options={{ placement: 'top', onHide: () => setLink(null) }}>
      {link !== null ? (
        <span className="doc-link-box">
          <input autoFocus value={link} placeholder="https://" onChange={e => setLink(e.target.value)}   // i18n-ignore
            onKeyDown={e => { if (e.key === 'Enter') { e.preventDefault(); apply() } if (e.key === 'Escape') setLink(null) }} />
          <button onMouseDown={e => e.preventDefault()} onClick={apply}>{t('确定')}</button>
        </span>
      ) : (
        <>
          <button className={editor.isActive('bold') ? 'on' : ''} title={t('粗体')} onClick={() => editor.chain().focus().toggleBold().run()}><b>B</b></button>
          <button className={editor.isActive('italic') ? 'on' : ''} title={t('斜体')} onClick={() => editor.chain().focus().toggleItalic().run()}><i>I</i></button>
          <button className={editor.isActive('strike') ? 'on' : ''} title={t('删除线')} onClick={() => editor.chain().focus().toggleStrike().run()}><s>S</s></button>
          <button className={editor.isActive('code') ? 'on' : ''} title={t('行内代码')} onClick={() => editor.chain().focus().toggleCode().run()}>{'</>'}</button>
          <button className={editor.isActive('link') ? 'on' : ''} title={t('链接')} onClick={() => setLink(editor.getAttributes('link').href || '')}>🔗</button>
        </>
      )}
    </BubbleMenu>
  )
}

/** 插图片：在光标那里弹一个填网址的小框（手机 App 里没有 prompt()）。 */
export function ImageBox({ editor, onClose }: { editor: Editor; onClose: () => void }) {
  const [url, setUrl] = useState('')
  const at = editor.view.coordsAtPos(editor.state.selection.from)
  const done = () => {
    const src = safeHref(url)
    if (src) editor.chain().focus().setImage({ src }).run()
    onClose()
  }
  return (
    <div className="doc-url-box" style={{ left: at.left, top: at.bottom + 6 }} data-no-swipe="">
      <input autoFocus value={url} placeholder={t('图片网址')} onChange={e => setUrl(e.target.value)}
        onKeyDown={e => { if (e.key === 'Enter') done(); if (e.key === 'Escape') onClose() }} onBlur={onClose} />
    </div>
  )
}
