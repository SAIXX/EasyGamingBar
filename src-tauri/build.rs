fn main() {
  let mut wattrs = tauri_build::WindowsAttributes::new();
  // release 包以管理员运行：PresentMon / 内置 ETW 的帧率测量需要提权令牌
  // （实测非管理员下会话能启动但收不到任何 Present 事件，静默无数据）。
  // dev 构建保持 asInvoker，开发工作流（npm run tauri dev）不受影响。
  if std::env::var("PROFILE").as_deref() == Ok("release") {
    wattrs = wattrs.app_manifest(
      r#"<assembly xmlns="urn:schemas-microsoft-com:asm.v1" manifestVersion="1.0">
  <dependency>
    <dependentAssembly>
      <assemblyIdentity
        type="win32"
        name="Microsoft.Windows.Common-Controls"
        version="6.0.0.0"
        processorArchitecture="*"
        publicKeyToken="6595b64144ccf1df"
        language="*"
      />
    </dependentAssembly>
  </dependency>
  <trustInfo xmlns="urn:schemas-microsoft-com:asm.v2">
    <security>
      <requestedPrivileges>
        <requestedExecutionLevel level="requireAdministrator" uiAccess="false" />
      </requestedPrivileges>
    </security>
  </trustInfo>
</assembly>"#,
    );
  }
  let attrs = tauri_build::Attributes::new().windows_attributes(wattrs);
  tauri_build::try_build(attrs).expect("failed to run tauri-build");
}
