/**
 * 看板刷新：一「拍」同时刷两处。
 *
 * 主区域的选中看板（`refreshSelectedBoard`）与侧栏各项目的摘要（`refreshBoards`）
 * 是两套数据。摘要原先只在握手时读一次，此后除非用户点进看板否则永远停在旧值——
 * 侧栏因此长期显示过期的「0 排队」。把两者绑进同一拍，侧栏才随队列真实变化。
 *
 * 两个刷新器各自吞掉自己的失败（它们内部的 try/catch 负责），这里不 await、
 * 不 catch：一拍绝不能被某处的读失败拖住。
 */
export function makeBoardTick(
  refreshSelectedBoard: () => Promise<unknown>,
  refreshBoards: () => Promise<unknown>,
): () => void {
  return () => {
    void refreshSelectedBoard();
    void refreshBoards();
  };
}
