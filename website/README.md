# rpi 官网

这是一个零依赖的静态官网。首页数据来自 `data/site.json`，Packages 目录数据来自 `data/packages.json`，Documentation 数据来自 `data/docs.json`，Blog 数据来自 `data/blog.json`；页面逻辑分别在 `app.js`、`packages.js`、`docs.js` 和 `blog.js`，共享 `styles.css`。`i18n.js` 和 `data/locales.json` 提供全站中文/英文切换，语言选择会保存在浏览器中。

在仓库根目录启动：

```powershell
python -m http.server 4173 --directory website
```

然后打开 <http://localhost:4173>、<http://localhost:4173/docs.html> 或 <http://localhost:4173/packages.html>。点击右上角 `EN` / `中` 可切换语言；也可以通过 `?lang=en` 或 `?lang=zh` 预设语言。文档页支持“使用文档 / SDK 文档”切换和章节搜索。

生产发布使用仓库根目录的 Taskfile：

```powershell
task rpi-deploy
```

全站六个页面均加载 `https://stat.laofu.online/script.js`，通过
`data-performance="true"` 开启真实用户性能采集，并在 tracker 之后加载
同站点 ID 的 `recorder.js`。回放是否启用、采样率、输入遮罩及最长录制时间
由 Umami 网站设置中的 Replays & Heatmaps 配置控制；添加 recorder 脚本后
仍须在后台开启 Replays。部署后检查两个脚本、公开 recorder 配置接口，以及
浏览器中的 `/api/send`（`type: "performance"`）与 `/api/record` 请求。

扩展目录的发布版本和近 90 天下载量来自 crates.io API；功能说明和当前源码
路径来自 `pi-rust/rpi-package` 的 `main` 分支。`data/packages-meta.json` 记录
核验时间、源码提交和目录数量。源码仓库中尚未发布到 crates.io 的扩展不提供
`rpi install <name>` 安装卡片。扩展页默认展示最近发布的扩展，可通过类型筛选
查看核心 crate。
