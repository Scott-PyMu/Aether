/**
 * 窗口化虚拟列表（M3-02；消息流虚拟滚动）。
 *
 * 口径：
 * - 固定行高（`itemHeight`）+ 固定视口（`height`）：可见区间 O(视口/行高)，
 *   总节点数不随 `items.length` 增长（10k 消息基准的帧预算保障）；
 * - 上下 `overscan` 行缓冲；滚动只更新 `scrollTop` 状态（不触发上游重渲染）；
 * - 高度与行高为显式属性（jsdom 无布局，测试可确定性驱动滚动）。
 */
import { useCallback, useState, type ReactNode } from "react";

export interface VirtualListProps<T> {
  items: T[];
  itemHeight: number;
  height: number;
  overscan?: number;
  renderItem: (item: T, index: number) => ReactNode;
  itemKey: (item: T, index: number) => string;
  testId?: string;
  className?: string;
  /** 滚动位置变化回调（诊断/测试）。 */
  onScrollTopChange?: (scrollTop: number) => void;
}

export function VirtualList<T>({
  items,
  itemHeight,
  height,
  overscan = 3,
  renderItem,
  itemKey,
  testId,
  className,
  onScrollTopChange,
}: VirtualListProps<T>) {
  const [scrollTop, setScrollTop] = useState(0);

  const onScroll = useCallback(
    (event: React.UIEvent<HTMLDivElement>) => {
      const next = event.currentTarget.scrollTop;
      setScrollTop(next);
      onScrollTopChange?.(next);
    },
    [onScrollTopChange],
  );

  const totalHeight = items.length * itemHeight;
  const first = Math.max(0, Math.floor(scrollTop / itemHeight) - overscan);
  const visibleCount = Math.ceil(height / itemHeight) + overscan * 2;
  const last = Math.min(items.length, first + visibleCount);
  const visible = items.slice(first, last);

  return (
    <div
      className={className}
      data-testid={testId}
      data-total-items={items.length}
      data-visible-items={visible.length}
      style={{ height, overflowY: "auto", position: "relative" }}
      onScroll={onScroll}
    >
      <div style={{ height: totalHeight, position: "relative" }}>
        {visible.map((item, index) => {
          const itemIndex = first + index;
          return (
            <div
              key={itemKey(item, itemIndex)}
              data-index={itemIndex}
              style={{
                position: "absolute",
                top: itemIndex * itemHeight,
                height: itemHeight,
                left: 0,
                right: 0,
              }}
            >
              {renderItem(item, itemIndex)}
            </div>
          );
        })}
      </div>
    </div>
  );
}
