// 资料库页面里“谁写的”（段、评论、历史版本的 by）：user（你）、bot:<id>、agent:<id>（外部 Agent，经 /mcp，见 osa/agentapi.py）。
// 编辑器、评论、历史、页面列表共用这一份，免得各认各的。按现在的 store 取名字：用到它的组件自己订阅 bots / agents 才会跟着刷新。
import { getState, useStore } from './store'
import { t } from './i18n'

export type Who = { name: string; color: string; agent?: boolean }

/** Bot 和外部 Agent 写的：名字和颜色；你写的、不知道是谁的：null。 */
export function who(by: string): Who | null {
  const { bots, agents, colors } = getState()
  let m = by.match(/^bot:(\d+)$/)
  if (m) {
    const b = bots.find(x => x.id === Number(m![1]))
    return { name: b?.name || t('已删除的 Bot'), color: b?.avatar.color || 'var(--fg3)' }
  }
  m = by.match(/^agent:(\d+)$/)
  if (m) {
    const id = Number(m[1])
    const a = agents.find(x => x.id === id)
    return { name: a?.name || t('一个外部 Agent'), color: colors.length ? colors[id % colors.length] : 'var(--fg3)', agent: true }
  }
  return null
}

/** 显示用的名字：你写的是“你”，不知道是谁的用 other。 */
export function nameOf(by: string, other = t('有人')): string {
  return by === 'user' ? t('你') : who(by)?.name ?? other
}

/** 组件里用：Bot 或外部 Agent 的名字变了跟着重新渲染。 */
export function useAuthors() {
  useStore(s => s.bots)
  useStore(s => s.agents)
  return nameOf
}
