/* SEH 守护：把覆盖层绘制包进 __try/__except。
 * 绘制途中任何访问违例（游戏设备中途失效、驱动 bug、2077 这类
 * D3D12+光追的边角情况）都会被拦下来返回异常码，而不是把游戏带崩。
 * 这是 OptiScaler/MangoApp 的绘制路径全部裹 SEH 的同款保命逻辑。 */
#include <windows.h>
#include <excpt.h>

typedef void (*egb_fn)(void *);

__declspec(noinline)
int egb_guard_seh(egb_fn f, void *arg) {
    __try {
        f(arg);
        return 0;
    } __except (EXCEPTION_EXECUTE_HANDLER) {
        return (int)GetExceptionCode();
    }
}
