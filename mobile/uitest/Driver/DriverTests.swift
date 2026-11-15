import XCTest

/// Drives the simulator UI for the push test: system alerts, lock button, tapping a notification.
/// Each test is one step; run.sh runs one with -only-testing. Text to look for comes from TEST_RUNNER_NOTIF_TEXT.
final class DriverTests: XCTestCase {
    let springboard = XCUIApplication(bundleIdentifier: "com.apple.springboard")
    var text: String { ProcessInfo.processInfo.environment["NOTIF_TEXT"] ?? "" }

    func testAllowNotifications() {
        let alert = springboard.alerts.firstMatch
        guard alert.waitForExistence(timeout: 30) else { return XCTFail("no permission alert") }
        for label in ["Allow", "允许"] where alert.buttons[label].exists {
            alert.buttons[label].tap(); return
        }
        XCTFail("alert has no Allow button: \(alert.debugDescription)")
    }

    func testLock() {
        XCUIDevice.shared.perform(NSSelectorFromString("pressLockButton"))
    }

    func testHome() {
        XCUIDevice.shared.press(.home)
    }

    func testWaitNotification() {
        let el = springboard.descendants(matching: .any).matching(NSPredicate(format: "label CONTAINS %@", text)).firstMatch
        XCTAssertTrue(el.waitForExistence(timeout: 60), "no element containing \(text)")
        print("FOUND: \(el.label)")
    }

    func testTapNotification() {
        let el = springboard.descendants(matching: .any).matching(NSPredicate(format: "label CONTAINS %@", text)).firstMatch
        guard el.waitForExistence(timeout: 60) else { return XCTFail("no element containing \(text)") }
        print("TAPPING: \(el.label)")
        el.tap()
    }

    /// Lock screen: tap the notification, then swipe up to unlock (no passcode), which completes the open.
    func testTapNotificationAndUnlock() {
        let el = springboard.descendants(matching: .any).matching(NSPredicate(format: "label CONTAINS %@", text)).firstMatch
        guard el.waitForExistence(timeout: 60) else { return XCTFail("no element containing \(text)") }
        print("TAPPING: \(el.label)")
        el.tap()
        sleep(1)
        let start = springboard.coordinate(withNormalizedOffset: CGVector(dx: 0.5, dy: 0.995))
        start.press(forDuration: 0.1, thenDragTo: springboard.coordinate(withNormalizedOffset: CGVector(dx: 0.5, dy: 0.35)))
    }

    func testSwipeUp() {
        let start = springboard.coordinate(withNormalizedOffset: CGVector(dx: 0.5, dy: 0.995))
        start.press(forDuration: 0.1, thenDragTo: springboard.coordinate(withNormalizedOffset: CGVector(dx: 0.5, dy: 0.35)))
    }

    // ---------- App 里的手势（左边缘右滑返回、不能缩放、整页不被拖着走）----------
    let osa = XCUIApplication(bundleIdentifier: "com.kadaliao.opensuperagent")
    func at(_ x: CGFloat, _ y: CGFloat) -> XCUICoordinate { osa.coordinate(withNormalizedOffset: CGVector(dx: x, dy: y)) }

    func testEdgeSwipe() {   // 从左边缘往右滑过大半个屏幕
        osa.activate()
        at(0.01, 0.5).press(forDuration: 0.05, thenDragTo: at(0.8, 0.5), withVelocity: 800, thenHoldForDuration: 0)
    }

    func testEdgeSwipeShort() {   // 只滑一小段、慢慢松手：应该弹回去
        osa.activate()
        at(0.01, 0.5).press(forDuration: 0.05, thenDragTo: at(0.15, 0.5), withVelocity: 60, thenHoldForDuration: 0.3)
    }

    func testPinch() {
        osa.activate()
        osa.webViews.firstMatch.pinch(withScale: 2.5, velocity: 3)
    }

    func testDoubleTap() {   // 文字里可以写 "x,y"，默认屏幕正中
        osa.activate()
        let n = nums
        n.count == 2 ? at(n[0], n[1]).doubleTap() : at(0.5, 0.5).doubleTap()
    }

    func testDragPage() {   // 在不能滚的地方（顶上的标题栏）往下拖
        osa.activate()
        at(0.5, 0.08).press(forDuration: 0.05, thenDragTo: at(0.5, 0.6), withVelocity: 600, thenHoldForDuration: 0.5)
    }

    var nums: [CGFloat] { text.split(separator: ",").compactMap { Double($0.trimmingCharacters(in: .whitespaces)) }.map { CGFloat($0) } }

    func testTap() {   // 文字里写 "x,y"（0–1，相对屏幕）
        osa.activate()
        let n = nums
        guard n.count == 2 else { return XCTFail("need x,y") }
        at(n[0], n[1]).tap()
    }

    func testDrag() {   // 文字里写 "x0,y0,x1,y1"
        osa.activate()
        let n = nums
        guard n.count == 4 else { return XCTFail("need x0,y0,x1,y1") }
        at(n[0], n[1]).press(forDuration: 0.05, thenDragTo: at(n[2], n[3]), withVelocity: 600, thenHoldForDuration: 0.5)
    }

    func testDump() {
        print(springboard.debugDescription)
    }

    // ---------- 编辑器：真的点键盘上的键（拼音键盘上打的就是拼音，走输入法的组字）、长按、按名字点 ----------
    func testKeys() {   // 文字里写要按的键，一个字符一个键；"␣" 是空格键（拼音键盘上是“选第一个候选”）
        osa.activate()
        let kb = osa.keyboards.firstMatch
        guard kb.waitForExistence(timeout: 5) else { return XCTFail("no keyboard") }
        for ch in text {
            let key = ch == "␣" ? kb.keys.matching(NSPredicate(format: "label IN %@", ["空格", "space", "选定", "确认"])).firstMatch : kb.keys[String(ch)]
            if key.exists { key.tap() } else { print("NOKEY: \(ch)") }
        }
    }

    func testKeysHome() {   // 打完字马上回桌面（自动保存还没来得及发）：看切到后台时补不补存
        testKeys()
        XCUIDevice.shared.press(.home)
    }

    func testKeyboard() {   // 键盘上有哪些键、候选
        osa.activate()
        print(osa.keyboards.firstMatch.debugDescription)
    }

    func testTapLabel() {   // 点 App 里（含键盘、系统菜单）文字等于这段的东西
        osa.activate()
        let el = osa.descendants(matching: .any).matching(NSPredicate(format: "label == %@", text)).firstMatch
        guard el.waitForExistence(timeout: 5) else { return XCTFail("nothing labelled \(text)") }
        print("TAPPING: \(el.label)")
        el.tap()
    }

    func testLongPress() {   // 文字里写 "x,y"：按住 1 秒（选词）
        osa.activate()
        let n = nums
        guard n.count == 2 else { return XCTFail("need x,y") }
        at(n[0], n[1]).press(forDuration: 1.0)
    }

    func testMenus() {   // 现在弹出来的菜单（系统的拷贝/粘贴、网页里的格式菜单）
        osa.activate()
        for q in [osa.menuItems, osa.menus, osa.buttons] {
            for el in q.allElementsBoundByIndex where el.isHittable { print("ITEM: \(el.elementType.rawValue) \(el.label) \(el.frame)") }
        }
    }
}
