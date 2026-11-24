import 'package:flutter/material.dart';

import 'theme.dart';

class BrandMark extends StatelessWidget {
  const BrandMark(this.size, {super.key});

  static const asset = 'assets/brand/mark-256.png';

  final double size;

  @override
  Widget build(BuildContext context) =>
      Image.asset(asset, width: size, height: size, excludeFromSemantics: true);
}

class AppHeader extends StatelessWidget implements PreferredSizeWidget {
  const AppHeader({
    super.key,
    required this.title,
    this.subtitle,
    this.leading,
    this.back = false,
    this.status,
    this.compact = false,
    this.actions = const [],
  });

  final String title;
  final String? subtitle;
  final Widget? leading;
  final bool back;
  final StatusPill? status;
  final bool compact;
  final List<Widget> actions;

  @override
  Size get preferredSize =>
      Size.fromHeight(compact && status != null ? 60 : 52);

  @override
  Widget build(BuildContext context) => Material(
    color: ConnectoColors.surface,
    child: SafeArea(
      bottom: false,
      child: Container(
        height: preferredSize.height,
        decoration: const BoxDecoration(
          border: Border(bottom: BorderSide(color: ConnectoColors.border)),
        ),
        padding: EdgeInsets.only(left: back ? 8 : 16, right: 16),
        child: Row(
          children: [
            if (back && Navigator.canPop(context)) ...[
              IconButton(
                tooltip: MaterialLocalizations.of(context).backButtonTooltip,
                onPressed: () => Navigator.maybePop(context),
                icon: const Icon(Icons.arrow_back),
              ),
              const SizedBox(width: 4),
            ],
            if (leading case final widget?) ...[
              widget,
              const SizedBox(width: 10),
            ],
            Expanded(
              child: Column(
                mainAxisAlignment: MainAxisAlignment.center,
                crossAxisAlignment: CrossAxisAlignment.start,
                children: [
                  Tooltip(
                    message: title,
                    excludeFromSemantics: true,
                    child: Text(
                      title,
                      maxLines: 1,
                      overflow: TextOverflow.ellipsis,
                      style: Theme.of(context).textTheme.titleMedium,
                    ),
                  ),
                  if (status case final pill? when compact)
                    Row(
                      children: [
                        Expanded(child: subtitleText(context)),
                        const SizedBox(width: 8),
                        pill,
                      ],
                    )
                  else
                    subtitleText(context),
                ],
              ),
            ),
            if (status case final pill? when !compact) ...[
              const SizedBox(width: 8),
              pill,
            ],
            if (actions.isNotEmpty) const SizedBox(width: 8),
            ...actions,
          ],
        ),
      ),
    ),
  );

  Widget subtitleText(BuildContext context) => subtitle == null
      ? const SizedBox.shrink()
      : Text(
          subtitle!,
          maxLines: 1,
          overflow: TextOverflow.ellipsis,
          style: Theme.of(context).textTheme.bodySmall,
        );
}

enum StatusTone { ok, busy, attention, error, neutral }

class StatusPill extends StatelessWidget {
  const StatusPill(
    this.label,
    this.tone, {
    super.key,
    required this.semanticsLabel,
  });

  final String label, semanticsLabel;
  final StatusTone tone;

  @override
  Widget build(BuildContext context) {
    final color = switch (tone) {
      StatusTone.ok => ConnectoColors.green,
      StatusTone.busy => ConnectoColors.accent,
      StatusTone.attention => ConnectoColors.amber,
      StatusTone.error => ConnectoColors.coral,
      StatusTone.neutral => ConnectoColors.muted,
    };
    return Semantics(
      label: semanticsLabel,
      excludeSemantics: true,
      child: DecoratedBox(
        decoration: BoxDecoration(
          color: color.withValues(alpha: 0.10),
          borderRadius: BorderRadius.circular(20),
        ),
        child: Padding(
          padding: const EdgeInsets.symmetric(horizontal: 8, vertical: 4),
          child: Row(
            mainAxisSize: MainAxisSize.min,
            children: [
              Icon(Icons.circle, size: 6, color: color),
              const SizedBox(width: 6),
              Flexible(
                child: Text(
                  label,
                  maxLines: 1,
                  overflow: TextOverflow.ellipsis,
                  style: Theme.of(context).textTheme.labelMedium
                      ?.copyWith(color: color),
                ),
              ),
            ],
          ),
        ),
      ),
    );
  }
}

enum NoticeTone { error, warn, info }

class NoticeBanner extends StatelessWidget {
  const NoticeBanner(
    this.message, {
    super.key,
    this.title,
    this.tone = NoticeTone.error,
    this.action,
  });

  final String message;
  final String? title;
  final NoticeTone tone;
  final Widget? action;

  @override
  Widget build(BuildContext context) {
    final (color, icon) = switch (tone) {
      NoticeTone.error => (ConnectoColors.coral, Icons.error_outline),
      NoticeTone.warn => (ConnectoColors.amber, Icons.warning_amber),
      NoticeTone.info => (ConnectoColors.accent, Icons.info_outline),
    };
    return DecoratedBox(
      decoration: BoxDecoration(
        color: color.withValues(alpha: 0.07),
        border: Border.all(color: color.withValues(alpha: 0.35)),
        borderRadius: BorderRadius.circular(8),
      ),
      child: Padding(
        padding: const EdgeInsets.all(12),
        child: Row(
          crossAxisAlignment: CrossAxisAlignment.start,
          children: [
            Icon(icon, size: 18, color: color),
            const SizedBox(width: 10),
            Expanded(
              child: Column(
                crossAxisAlignment: CrossAxisAlignment.start,
                children: [
                  if (title case final line?)
                    Text(line, style: Theme.of(context).textTheme.titleSmall),
                  SelectableText(message),
                  ?action,
                ],
              ),
            ),
          ],
        ),
      ),
    );
  }
}

class SectionCard extends StatelessWidget {
  const SectionCard({super.key, required this.child});

  final Widget child;

  @override
  Widget build(BuildContext context) => Material(
    color: ConnectoColors.raised,
    shape: const RoundedRectangleBorder(
      side: BorderSide(color: ConnectoColors.border),
      borderRadius: BorderRadius.all(Radius.circular(12)),
    ),
    clipBehavior: Clip.antiAlias,
    child: child,
  );
}

class RowAction extends StatefulWidget {
  const RowAction({super.key, this.onTap, required this.child});

  final VoidCallback? onTap;
  final Widget child;

  @override
  State<RowAction> createState() => _RowActionState();
}

class _RowActionState extends State<RowAction> {
  bool focused = false;

  @override
  Widget build(BuildContext context) => Semantics(
    button: true,
    enabled: widget.onTap != null,
    child: InkWell(
      onTap: widget.onTap,
      onFocusChange: (value) => setState(() => focused = value),
      hoverColor: ConnectoColors.hover,
      child: Container(
        constraints: const BoxConstraints(minHeight: 44),
        alignment: Alignment.center,
        decoration: BoxDecoration(
          border: Border.all(
            color: focused ? ConnectoColors.accent : Colors.transparent,
            width: 2,
            strokeAlign: BorderSide.strokeAlignInside,
          ),
          borderRadius: BorderRadius.circular(8),
        ),
        padding: const EdgeInsets.symmetric(horizontal: 12, vertical: 6),
        child: widget.child,
      ),
    ),
  );
}
