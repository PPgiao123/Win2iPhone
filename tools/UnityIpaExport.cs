using System;
using System.Linq;
using UnityEditor;
using UnityEditor.Build.Reporting;

public static class UnityIpaWorkflowExport
{
    public static void Run()
    {
        var scenes = EditorBuildSettings.scenes
            .Where(scene => scene.enabled && !string.IsNullOrEmpty(scene.path))
            .Select(scene => scene.path)
            .ToArray();
        if (scenes.Length == 0)
        {
            throw new InvalidOperationException("Enable at least one scene in the Unity build scene list.");
        }

        var output = Environment.GetEnvironmentVariable("UNITY_IPA_XCODE_OUTPUT");
        if (string.IsNullOrEmpty(output))
        {
            throw new InvalidOperationException("The iOS export destination is missing.");
        }
        var report = BuildPipeline.BuildPlayer(new BuildPlayerOptions
        {
            scenes = scenes,
            locationPathName = output,
            target = BuildTarget.iOS,
            options = BuildOptions.None
        });
        if (report.summary.result != BuildResult.Succeeded)
        {
            throw new InvalidOperationException("Unity iOS export failed with " + report.summary.totalErrors + " errors.");
        }
    }
}
